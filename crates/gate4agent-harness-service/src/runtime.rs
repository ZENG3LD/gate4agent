use crate::{
    c2::{
        AcceptedSpawnBindingProof, ActivatedHarnessMcpReservationProof,
        ArmedHarnessMcpReservationProof,
        ContextPackExportStart, ExportContextPackOutcome, HarnessC2Adapter,
        HarnessC2Error, HarnessC2EventReceiver,
        HarnessObservationResync, PendingHostDirectoryBrowse, PendingNativeHistoryRequest,
        PendingNodeWorkspaceRead,
        PendingNodeWorkspaceWrite, PendingResourceMutation, PendingRunRead,
        PendingRunContextSourceObservation, PendingSessionControl,
        PendingSessionRecordMutation, PreparedHostDirectoryBrowse,
        PreparedRunContextSourceObservation,
        PreparedNodeWorkspaceRead, PreparedNodeWorkspaceWrite, PreparedResourceMutation,
        PreparedRunRead, PreparedSessionControl, PreparedSessionRecordMutation,
        PreparedSessionSpawn,
        RunContextSourceObservationCompletion,
        RunContextSourceProjection, RunReadCompletion, WorkspaceReadKind,
        ManagedWorktreeSpawnDispatchOutcome, PendingManagedWorktreeSpawnDispatch,
        SessionRosterEffect,
        SpawnDispatchOutcome, SpawnProfileRevisionProof,
        StagedDeliveryProof,
    },
    credential::{CredentialAuthority, CredentialBindingV1, CredentialError},
    read::{
        execute_exact_binding_read, execute_operator_monitor, execute_operator_timeline,
        execute_read, verify_observation_credential_binding,
    },
    mutation_request_digest,
    HarnessApplyOutcome, HarnessMutationV1, HarnessService, HarnessServiceError,
    PreparedScheduledSpawnLease,
};
use crate::dispatch::{
    deterministic_default_grant_ids, deterministic_dispatch_ids, deterministic_issued_dispatch_ids,
    deterministic_lifecycle_authority_ids,
    derive_launch_plans_from_inventory,
    exact_bound_control_lifecycle,
    HarnessLaunchCatalog, HarnessLifecycleEventKindV1, HarnessLifecycleProjectionV1,
    HARNESS_LAUNCH_CATALOG_MAX,
};
use crate::agent_stream::{
    AgentStreamSubscriberRegistry, HOST_AGENT_STREAM_SUBSCRIBER_LIMIT,
    HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY,
};
use crate::terminal::{
    map_screen_state, terminal_frame_to_wire, TerminalBufferRegistry, TerminalSubscriberRegistry,
    HOST_TERMINAL_SUBSCRIBER_LIMIT, HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY,
};
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_c2_protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2ObservationSupport, C2SessionStatus,
    NodeRoute, RoutedNodeEvent,
};
use gate4agent_observation_api::{
    ManagedRecordLink, ManagedSessionKey, NodeCursor, NodeId, NodeIncarnationId,
    ObservationGap, ObservationIngressEnvelope, ObservationIngressPayload,
    ObservationResyncBatch, ObservationTarget, ObservationTransport, RuntimeSessionKey,
};
use gate4agent_observation_service::{ObservationService, ObservationServiceError};
use gate4agent_harness_api::{
    HarnessInlineRunSessionV1, HarnessManagedRunSessionV1,
    HarnessLaunchPlanPageV1, HarnessLaunchPlanSummaryV1,
    HarnessNodeIncarnationV1,
    HarnessOperatorAgentEventV1,
    HarnessOperatorApiError,
    HarnessOperatorCredential, HarnessOperatorEnvelopeV1, HarnessOperatorEventV1,
    HarnessOperatorHostErrorV1, HarnessOperatorTerminalEventV1,
    HarnessOperatorIntentV1,
    HarnessOperatorMutationOutcomeV1, HarnessOperatorReplyV1, HarnessOperatorRequestV1,
    HarnessOperatorResponseV1, HarnessReadCredential, HarnessReadEnvelopeV1,
    HarnessIssuedExecutionSpecSummaryV1, HarnessManagedWorktreeProfileOptionV1,
    HarnessManagedWorktreeRetentionV1, HarnessOrdinaryLaunchPlanOptionV1,
    HarnessRunContextTransferV1, HarnessRunContinuationTransferV1,
    HarnessRunContextSourceObservationV1,
    HarnessRunCorrelationAvailabilityV1, HarnessRunCorrelationV1,
    HarnessRunDeliveryTransferV1, HarnessRunTransferSummaryV1,
    HarnessRunSessionViewV1, HarnessRunWorktreeViewV1,
    HarnessRuntimeInventoryPageV1, HarnessRuntimeInventoryV1,
    HarnessRuntimeManagedModeV1, HarnessRuntimeManagedSessionV1,
    HarnessRuntimeManagedStateV1, HarnessRuntimeNodeInventoryV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeSessionBindingV1,
    HarnessRuntimeSessionStatusV1,
    HarnessRuntimeSessionV1, HarnessRuntimeTerminalPageV1, HarnessRuntimeTerminalSizeV1,
    HarnessRuntimeTransportV1, HarnessRuntimeWorkspaceV1,
    HarnessRuntimeBundleReceiptV1, HarnessRuntimeEnvironmentProfileReceiptV1,
    HarnessRuntimeLaunchInventoryV1, HarnessRuntimeSpawnProfileSummaryV1,
    HarnessTaskLaunchOptionsV1,
    HarnessReverseAttributionBindingV1, HarnessReverseAttributionLinkV1,
    HarnessReverseAttributionOutcomeV1, HarnessReverseAttributionRelationV1,
    HarnessReverseAttributionSubjectV1, HarnessReverseAttributionV1,
    FeatureObservationStateV1, ProjectionAvailabilityV1, ProjectionFreshnessV1,
    HarnessReadHostErrorV1, HarnessReadReplyV1, RedactedBindingStateV1,
    RedactedRunIntentV1, RedactedRunV1, RedactedTaskV1, RedactedWorktreeIntentV1,
    RunPageV1, TaskCreatorCategoryV1, TaskPageV1,
    HARNESS_OPERATOR_RESPONSE_MAX_BYTES, HARNESS_READ_REQUEST_MAX_BYTES,
    HARNESS_READ_RESPONSE_MAX_BYTES,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessDispatchIntentV1, HarnessExecutionModeV1,
    HarnessFailureCategoryV1,
    HarnessIdempotencyRef,
    HarnessFailureV1, HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationV1,
    HarnessOutcomeUnknownReasonV1, HarnessResultDispositionV1, HarnessResultRef, HarnessRevision,
    HarnessRunGitFactsOutcomeV1, HarnessRunGitFactsV1, HarnessRunGitCommitSummaryV1,
    HarnessRunGitStatusCodeV1, HarnessRunGitStatusEntryV1, HarnessRunGitSummaryV1,
    HarnessRunLifecycleV1, HarnessRunV1, HarnessSelectorV1, HarnessTaskStateV1, HarnessTaskV1,
    HarnessRuntimeIdentityV1, HarnessSessionBindingV1, HarnessSessionIdentityV1,
    HarnessContextSourceSelectionV1, HarnessContextSourceAvailabilityV1, HarnessRequestDigest,
    HarnessWorktreeIntentV1, HarnessContinuationV1, HarnessDeliveryV1,
    HarnessTransferAuthorityRefV1,
    HarnessGrantTargetV1, SessionGrantV1,
};
use gate4agent_node_wire::{local_hmac_sha256, proofs_match};
use gate4agent_node_protocol::{
    HarnessMcpActivationDigest, HarnessMcpCallId, HarnessMcpLocalReplyV1,
    HarnessMcpRejectReasonV1, HarnessMcpReplyChunkHexV1, HarnessMcpReservationId,
    NodeFailureCode, SessionAddress,
    SessionRecordId, SpawnBundleId, SpawnContextId, SpawnProfileId,
    MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES, MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES,
    MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, Semaphore},
    task::JoinHandle,
    time::{interval_at, timeout, Instant, MissedTickBehavior},
};

const HOST_COMMAND_CAPACITY: usize = 64;
const HOST_CONNECTION_LIMIT: usize = 32;
// A subscribed connection is long-lived by design (it never sees
// `HOST_CONNECTION_DEADLINE`), so it must not compete with ordinary
// one-shot requests for the shared `HOST_CONNECTION_LIMIT` pool -- a leaked
// or hung subscriber would otherwise permanently shrink the ordinary
// concurrent-request headroom. Its own, much smaller cap lives here instead.
const HOST_SUBSCRIBER_LIMIT: usize = 8;
// `SubscriberRegistry` only ever discovers a dead subscriber when a write to
// it fails (see that struct's own doc comment) -- there is no read/EOF
// detection on the subscription's `TcpStream` and no subscription lifetime.
// A subscriber that abandons its own connection while the harness has
// nothing new to push therefore sits occupying its registry entry and its
// `HOST_SUBSCRIBER_LIMIT` permit indefinitely: confirmed in practice, an
// ordinary, continuously-running `gate4agent-tui` self-abandons and
// re-subscribes roughly every 120s on its own, with zero dependency on any
// real task/run/inventory event ever happening (docs/gate4agent/research/
// gate4agent-operator-subscriber-slot-leak-2026-08-25.md). At
// `HOST_SUBSCRIBER_LIMIT` = 8 and that ~120s cadence, 8 such abandonments
// exhaust the whole pool in about 16 minutes from an otherwise-healthy
// client doing nothing wrong by the wire's own rules. This interval drives
// the periodic keep-alive tick (`emit_subscriber_keepalive`) that makes a
// write attempt happen regardless of real activity, so `emit`'s existing
// `Closed` handling reaps an abandoned subscriber promptly instead of
// waiting on the next real event. It must stay well under the ~120s/8-slot
// budget above to guarantee the pool cannot exhaust from idle abandonment
// alone; 30s clears that bar with several ticks of margin (four per ~120s
// window, not one) while staying far below being a cost of its own -- one
// `try_send` per live subscriber, twice a minute, is not a meaningfully
// different load than the `emit` calls a single ordinary task/run change
// already causes.
const HOST_SUBSCRIBER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
// Bounded per-subscriber outbound queue: `SubscriberRegistry::emit` uses
// `try_send`, never blocking the single-writer select loop on a slow
// reader. A subscriber whose queue fills up is marked `needs_baseline`
// (see `HarnessEventSubscriber`) rather than back-pressuring event
// application.
const HOST_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;
const HOST_DEADLINE: Duration = Duration::from_secs(3);
const HOST_NATIVE_HISTORY_RESPONSE_DEADLINE: Duration = Duration::from_secs(40);
const HOST_RUN_READ_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
const HOST_RUN_CONTEXT_SOURCE_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
// `NodeRequest::Input`/`Resize` fall into c2-client's default 10s relay
// deadline, `Stop` gets an explicit 15s; both get `RELAY_REPLY_HEADROOM`
// (5s) on top (`control_request_deadline`, gate4agent-c2-client/runtime.rs).
// This outer bound stays above the worst case (Stop, 20s) with margin, per
// the "outer bound never below inner timeout" discipline the other harness
// read families already follow above.
const HOST_SESSION_CONTROL_RESPONSE_DEADLINE: Duration = Duration::from_secs(22);
// `NodeRequest::SpawnSpec`'s relay deadline is the spec's own
// `deadline_ms` (`SESSION_SPAWN_DEADLINE_MS` in c2.rs, 20s) plus the same 5s
// `RELAY_REPLY_HEADROOM` -- 25s inner. This outer bound stays above that.
const HOST_SESSION_SPAWN_RESPONSE_DEADLINE: Duration = Duration::from_secs(28);
// `ResumeSessionRecord` spawns a new process the same way `SpawnSpec` does,
// so this family's outer bound matches `HOST_SESSION_SPAWN_RESPONSE_DEADLINE`
// rather than the thinner `HOST_SESSION_CONTROL_RESPONSE_DEADLINE`; the other
// five verbs in the family (rename/set-task/forget/index-provider/
// index-native) are cheap store mutations that settle well inside it.
const HOST_SESSION_RECORD_MUTATION_RESPONSE_DEADLINE: Duration = Duration::from_secs(28);
// Same class as `HOST_RUN_READ_RESPONSE_DEADLINE`: a folder-browser page is a
// fast local directory listing, not a spawn or a git operation.
const HOST_DIRECTORY_BROWSE_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
// `CreateWorktree`/`RemoveWorktree` shell out to git (comparable cost to a
// fresh process spawn); the other five verbs in this family (register/
// unregister/create-standalone-workspace, export/forget-context-pack) are
// cheap by comparison but share this same bound the same way the session-
// record-mutation family's own five cheap verbs share `ResumeSessionRecord`'s
// bound -- see that constant's own doc comment for the identical reasoning.
const HOST_RESOURCE_MUTATION_RESPONSE_DEADLINE: Duration = Duration::from_secs(28);
const HOST_CONNECTION_DEADLINE: Duration = Duration::from_secs(45);
const OBSERVATION_RECOVERY_RETRY: Duration = Duration::from_secs(1);
const OBSERVATION_RECOVERY_MAX_IN_FLIGHT: usize = 8;
const OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX: usize = 64;
const OBSERVATION_RECOVERY_BUFFERED_BYTES_MAX: usize = 1024 * 1024;
const HARNESS_MCP_ABORT_RETRY_MAX_MS: u64 = 30_000;
const HARNESS_MCP_NETWORK_WORKERS_MAX: usize = 8;
const HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX: usize =
    HARNESS_MCP_NETWORK_WORKERS_MAX - 1;
const NATIVE_HISTORY_WORKERS_MAX: usize = 8;
const RUN_READ_WORKERS_MAX: usize = 8;
// Deliberately a separate pool from `RUN_READ_WORKERS_MAX`, not shared: a
// burst of harness-mode sidebar Files/Git reads (no run in flight) must
// never be able to starve a live operator's run-scoped
// `InspectRunWorkspace` by soaking up the shared pool.
const NODE_WORKSPACE_READ_WORKERS_MAX: usize = 8;
// Own pool, not shared with `NODE_WORKSPACE_READ_WORKERS_MAX`: an editor
// save or a file/directory creation must never be able to starve a
// concurrent sidebar Files/Git read (or vice versa) by soaking up the same
// pool -- same isolation rationale as that constant's own doc comment.
const NODE_WORKSPACE_WRITE_WORKERS_MAX: usize = 8;
// Own pools, not shared with `NODE_WORKSPACE_READ_WORKERS_MAX`: a burst of
// session-control traffic (keystrokes, resizes) must never be able to starve
// a concurrent node-workspace read or vice versa.
const SESSION_SPAWN_WORKERS_MAX: usize = 8;
const SESSION_CONTROL_WORKERS_MAX: usize = 8;
// Own pool, not shared with `SESSION_CONTROL_WORKERS_MAX`: a burst of
// session-record mutations (rename, set-task, forget, index-provider,
// index-native, resume-session-record) must never be able to starve a
// concurrent live-session keystroke/resize, or vice versa.
const SESSION_RECORD_MUTATION_WORKERS_MAX: usize = 8;
// Own pool, not shared with `NODE_WORKSPACE_READ_WORKERS_MAX`: a burst of
// folder-browser paging must never be able to starve a concurrent sidebar
// Files/Git read, or vice versa -- same isolation rationale as that
// constant's own doc comment.
const HOST_DIRECTORY_BROWSE_WORKERS_MAX: usize = 8;
// Own pool, not shared with `SESSION_RECORD_MUTATION_WORKERS_MAX`: a burst of
// resource mutations (workspace/worktree lifecycle, context-pack export/
// forget) must never be able to starve a concurrent session-record mutation,
// or vice versa.
const RESOURCE_MUTATION_WORKERS_MAX: usize = 8;
const RUN_CONTEXT_SOURCE_WORKERS_MAX: usize = 8;
// Deliberately separate from `RUN_READ_WORKERS_MAX`, not shared: background
// git-facts capture must never be able to starve a live operator's own
// `InspectRunWorkspace` click by soaking up the shared pool during a burst
// of run completions (A3 design §3.3).
const RUN_GIT_FACTS_WORKERS_MAX: usize = 2;
// Bounded latency on a background fact, not a correctness requirement — see
// the A3 design §3.4/§9 risk 1 for why this is periodic rather than
// reactive.
const RUN_GIT_FACTS_SWEEP_PERIOD: Duration = Duration::from_secs(5);
const RUN_CONTEXT_SOURCE_TOTAL_BUDGET: Duration = Duration::from_secs(11);
const RUN_CONTEXT_SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const OPERATOR_CREDENTIAL_DIGEST_DOMAIN: &[u8] = b"gate4agent-harness-operator-credential-digest-v1";
const OPERATOR_INTENT_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-operation-id-v1";
const OPERATOR_INTENT_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-idempotency-ref-v1";
const OPERATOR_INTENT_TASK_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-task-id-v1";
// A direct `SpawnSession` has no CAS/replay layer (see the doc comment on
// `HarnessOperatorRequestV1::SpawnSession`): these domains mint a fresh,
// host-local nonce pair per dispatch purely to satisfy
// `PreparedSpawnDispatch::new`'s wire-correlation identity, never to derive
// a stable id a resubmission could reproduce -- see
// `mint_session_spawn_ids`.
const SESSION_SPAWN_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-session-spawn-operation-id-v1";
const SESSION_SPAWN_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-session-spawn-idempotency-ref-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarnessHostEndpoint(SocketAddr);

#[derive(Clone, Debug, Default)]
pub struct HarnessRuntimeCatalogs {
    pub launch: HarnessLaunchCatalog,
    pub delivery: DeliveryCatalogV2,
}

impl HarnessRuntimeCatalogs {
    pub fn new(
        launch: HarnessLaunchCatalog,
        delivery: DeliveryCatalogV2,
    ) -> Result<Self, HarnessRuntimeError> {
        launch.validate_delivery_catalog(&delivery)
            .map_err(|_| HarnessRuntimeError::LaunchCatalog)?;
        Ok(Self { launch, delivery })
    }
}

impl HarnessHostEndpoint {
    pub fn socket_addr(self) -> SocketAddr { self.0 }
}

#[derive(Clone)]
pub struct HarnessHostHandle {
    endpoint: HarnessHostEndpoint,
    commands: mpsc::Sender<HostCommand>,
}

impl HarnessHostHandle {
    pub fn endpoint(&self) -> HarnessHostEndpoint { self.endpoint }

    pub async fn mint_credential(
        &self,
        binding: CredentialBindingV1,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<HarnessReadCredential, HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::Mint {
            binding,
            issued_at_unix_ms,
            expires_at_unix_ms,
            reply,
        }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn shutdown(&self) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::Shutdown { reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn apply_harness_mutation(
        &self,
        mutation: HarnessMutationV1,
    ) -> Result<HarnessApplyOutcome, HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::ApplyHarnessMutation { mutation, reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn activate_harness_mcp(
        &self,
        reservation_id: HarnessMcpReservationId,
    ) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::ActivateHarnessMcp { reservation_id, reply })
            .await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn revoke_harness_mcp(
        &self,
        reservation_id: HarnessMcpReservationId,
    ) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::RevokeHarnessMcp { reservation_id, reply })
            .await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }
}

enum HostCommand {
    Mint {
        binding: CredentialBindingV1,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        reply: oneshot::Sender<Result<HarnessReadCredential, HarnessRuntimeError>>,
    },
    Read {
        envelope: HarnessReadEnvelopeV1,
        reply: oneshot::Sender<HarnessReadReplyV1>,
    },
    Operator {
        request: HarnessOperatorRequestV1,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        /// Signals when `handle_connection`'s own `response_deadline` fires
        /// before a reply arrives. Only ever populated for a node-workspace
        /// read, node-workspace write, session-spawn, session-control,
        /// session-record-mutation, host-directory-browse, or resource-
        /// mutation request (see `start_node_workspace_read_worker`/
        /// `start_node_workspace_write_worker`/`start_session_spawn_worker`/
        /// `start_session_control_worker`/`start_session_record_mutation_
        /// worker`/`start_host_directory_browse_worker`/`start_resource_
        /// mutation_worker`); every other request kind leaves this `None` and
        /// the worker that eventually handles it ignores it.
        /// `None` on the two constructed-in-tests call sites means "behaves
        /// exactly as before this field existed" — no cancellation
        /// available, not an error.
        cancel: Option<oneshot::Receiver<()>>,
    },
    ApplyHarnessMutation {
        mutation: HarnessMutationV1,
        reply: oneshot::Sender<Result<HarnessApplyOutcome, HarnessRuntimeError>>,
    },
    ActivateHarnessMcp {
        reservation_id: HarnessMcpReservationId,
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
    RevokeHarnessMcp {
        reservation_id: HarnessMcpReservationId,
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
    DispatchPreflightFinished {
        intent: HarnessDispatchIntentV1,
        result: Result<SpawnProfileRevisionProof, HarnessC2Error>,
    },
    DispatchFinished {
        operation_id: HarnessOperationId,
        result: CoordinatorSpawnResult,
    },
    DeliveryStageFinished {
        operation_id: HarnessOperationId,
        result: Result<StagedDeliveryProof, HarnessC2Error>,
    },
    ContinuationExportFinished {
        operation_id: HarnessOperationId,
        result: Result<ExportContextPackOutcome, HarnessC2Error>,
    },
    HarnessMcpArmFinished {
        operation_id: HarnessOperationId,
        spec: gate4agent_node_protocol::SpawnSpec,
        profile: SpawnProfileRevisionProof,
        result: Result<ArmedHarnessMcpReservationProof, HarnessC2Error>,
    },
    HarnessMcpActivationFinished {
        reservation_id: HarnessMcpReservationId,
        attempt_id: u64,
        expected_revision: HarnessRevision,
        result: Result<ActivatedHarnessMcpReservationProof, HarnessC2Error>,
    },
    HarnessMcpAbortFinished {
        reservation_id: HarnessMcpReservationId,
        attempt_id: u64,
        result: Result<(), HarnessC2Error>,
    },
    HarnessMcpRelayFinished {
        reservation_id: HarnessMcpReservationId,
        call_id: HarnessMcpCallId,
        attempt_id: u64,
        result: Result<(), HarnessRuntimeError>,
    },
    ObservationRecoveryFinished {
        route: NodeRoute,
        attempt_id: u64,
        requested_after: u64,
        result: Result<HarnessObservationResync, HarnessC2Error>,
    },
    NativeHistoryWorkerFinished,
    RunReadFinished {
        completion: RunReadCompletion,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
    },
    NodeWorkspaceReadFinished {
        result: Result<HarnessOperatorResponseV1, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
    },
    NodeWorkspaceWriteFinished {
        result: Result<HarnessOperatorResponseV1, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
    },
    SessionSpawnFinished {
        route: NodeRoute,
        result: Result<SpawnDispatchOutcome, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
        /// The transport this exact request asked for -- captured from
        /// `PreparedSessionSpawn::mode` before `dispatch_session_spawn`
        /// consumed it, so a `SpawnDispatchOutcome::Rejected` can name it on
        /// the operator wire (see `map_session_spawn_node_failure`).
        requested_transport: HarnessRuntimeTransportV1,
    },
    SessionControlFinished {
        result: Result<(), HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
        route: NodeRoute,
        /// `SessionRosterEffect::None` unless the verb settled successfully
        /// -- captured from `PendingSessionControl::roster_effect` before it
        /// was consumed (see that method's doc comment), so a successful
        /// `Stop`/`Remove`/`Resume` can record the roster-invalidation
        /// (and, for `Stop`/`Remove`, the "this session must be gone")
        /// expectation on the route's recovery entry.
        roster_effect: SessionRosterEffect,
    },
    /// Every session-record mutation invalidates the route's cached
    /// runtime-inventory entry on success (see the handler below) --
    /// unconditionally, unlike `SessionControlFinished`'s per-verb
    /// `SessionRosterEffect`: all six verbs in this family mutate the node's
    /// managed-session store the runtime inventory's `managed_sessions`
    /// roster caches, so there is no verb here with `SessionRosterEffect::
    /// None`'s "cannot possibly change the roster" property.
    SessionRecordMutationFinished {
        result: Result<HarnessOperatorResponseV1, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
        route: NodeRoute,
    },
    HostDirectoryBrowseFinished {
        result: Result<HarnessOperatorResponseV1, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
    },
    /// See `ResourceMutationKind::invalidates_runtime_inventory`'s doc
    /// comment (`c2.rs`) for which of the seven verbs in this family set
    /// `invalidates_runtime_inventory` true -- captured from
    /// `PendingResourceMutation::invalidates_runtime_inventory` before it was
    /// consumed, the same timing `SessionControlFinished`'s `roster_effect`
    /// uses.
    ResourceMutationFinished {
        result: Result<HarnessOperatorResponseV1, HarnessC2Error>,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
        identity: OperatorRequestLogIdentity,
        route: NodeRoute,
        invalidates_runtime_inventory: bool,
    },
    RunGitFactsCaptureFinished {
        run_id: gate4agent_harness_protocol::HarnessRunId,
        completion: RunReadCompletion,
    },
    RunContextSourceFinished {
        completion: RunContextSourceObservationCompletion,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
    },
    /// Registers a new event subscriber. Fire-and-forget: unlike every other
    /// variant carrying a `reply`, there is no ack here -- the connection
    /// task already holds the paired `mpsc::Receiver`, and the loop's own
    /// `SnapshotBaseline` push (sent to `sender` the moment this arm runs)
    /// is itself the observable proof of successful registration. See
    /// `handle_connection`'s `SubscribeEvents` branch for the sender side.
    Subscribe {
        sender: mpsc::Sender<HarnessOperatorEventV1>,
        identity: OperatorRequestLogIdentity,
    },
    /// Registers a new terminal-push subscriber -- the sibling of `Subscribe`
    /// above for `TerminalSubscriberRegistry` rather than `SubscriberRegistry`.
    /// Also fire-and-forget for the same reason: the connection task already
    /// holds the paired `mpsc::Receiver`, and this arm's own per-session seed
    /// (via `TerminalSubscriberRegistry::send_to`, using whatever
    /// `TerminalBufferRegistry::latest` already holds for each requested
    /// session) is the observable proof of successful registration. See
    /// `handle_connection`'s `SubscribeTerminal` branch for the sender side.
    SubscribeTerminal {
        sender: mpsc::Sender<HarnessOperatorTerminalEventV1>,
        sessions: HashSet<RuntimeSessionKey>,
        identity: OperatorRequestLogIdentity,
    },
    /// Registers a new agent-stream-push subscriber -- the sibling of
    /// `SubscribeTerminal` immediately above for `AgentStreamSubscriberRegistry`
    /// rather than `TerminalSubscriberRegistry`. Also fire-and-forget, but
    /// unlike `SubscribeTerminal`'s explicit per-session `send_to` call, the
    /// seed here is folded into `AgentStreamSubscriberRegistry::insert`
    /// itself: it hands the new subscriber whatever `ModeCatalog`/
    /// `ConfigOptions`/`ModelCatalog`/unresolved `InteractionPrompt` it
    /// already holds for the requested sessions, before any live chunk (see
    /// `agent_stream.rs`'s own module doc comment for the three-way seeding
    /// split this follows). See `handle_connection`'s `SubscribeAgentStream`
    /// branch for the sender side.
    SubscribeAgentStream {
        sender: mpsc::Sender<HarnessOperatorAgentEventV1>,
        sessions: HashSet<RuntimeSessionKey>,
        identity: OperatorRequestLogIdentity,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
}

/// Outcome of one attempted push to a single subscriber's outbound channel.
/// Shared by every call site in `SubscriberRegistry` so `Full`/`Closed`
/// handling never drifts between `emit`, `send_to`, and `recover_lagged`.
enum SubscriberSendOutcome {
    Sent,
    Full,
    Closed,
}

/// One connection's worth of push-event subscription state, owned entirely
/// by the select loop. Registered via `HostCommand::Subscribe`; pruned the
/// moment its `sender` reports `Closed` (the connection task ended,
/// including a normal client-side unsubscribe-by-disconnect).
struct HarnessEventSubscriber {
    id: u64,
    sender: mpsc::Sender<HarnessOperatorEventV1>,
    /// Set by `emit` when a `try_send` finds the queue full: the event that
    /// overflowed it is dropped for this subscriber only (every other live
    /// subscriber is unaffected). Cleared only once `recover_lagged` lands
    /// both a `Lagged` and the `SnapshotBaseline` that must follow it.
    needs_baseline: bool,
    /// Per-subscription monotonic; never resets, including across a
    /// `Lagged`/`SnapshotBaseline` pair -- see `HarnessOperatorEventV1`'s
    /// doc comment.
    next_sequence: u64,
    identity: OperatorRequestLogIdentity,
}

impl HarnessEventSubscriber {
    fn try_send(
        &mut self,
        build: impl FnOnce(u64) -> HarnessOperatorEventV1,
    ) -> SubscriberSendOutcome {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        match self.sender.try_send(build(sequence)) {
            Ok(()) => SubscriberSendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SubscriberSendOutcome::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => SubscriberSendOutcome::Closed,
        }
    }
}

/// Loop-local registry of live event subscribers. A `Vec`, not a map:
/// subscriber count is capped tiny (`HOST_SUBSCRIBER_LIMIT`) by a dedicated
/// semaphore before a connection ever reaches `HostCommand::Subscribe`, so
/// linear scan/removal costs nothing observable.
///
/// Promoted `pub` for `gate4agent-harness-light` (A3): every method here
/// (`insert`/`is_empty`/`send_to`/`emit`/`needs_recovery`/`recover_lagged_with`)
/// works purely in terms of `mpsc::Sender<HarnessOperatorEventV1>` and
/// `OperatorRequestLogIdentity` -- no `HarnessService`/kernel entanglement --
/// so the light harness reuses this registry verbatim rather than
/// reimplementing the overflow/lag-recovery/pruning state machine
/// light-local. Only `recover_lagged` itself (below, still private) stays
/// tied to this crate's own `HarnessService`/`HarnessRuntimeInventoryCache`
/// baseline; `recover_lagged_with` is the kernel-free generalization both
/// harnesses' own loops call.
#[derive(Default)]
pub struct SubscriberRegistry {
    subscribers: Vec<HarnessEventSubscriber>,
    next_id: u64,
}

impl SubscriberRegistry {
    pub fn insert(
        &mut self,
        sender: mpsc::Sender<HarnessOperatorEventV1>,
        identity: OperatorRequestLogIdentity,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.subscribers.push(HarnessEventSubscriber {
            id,
            sender,
            needs_baseline: false,
            next_sequence: 0,
            identity,
        });
        id
    }

    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty()
    }

    fn remove_at(&mut self, index: usize) {
        let removed = self.subscribers.swap_remove(index);
        tracing::info!(
            subscriber_id = removed.id,
            operation = %removed.identity.operation,
            node_id = removed.identity.node_id(),
            workspace_id = removed.identity.workspace_id(),
            "harness operator event subscriber closed",
        );
    }

    /// Pushes to exactly one subscriber by id -- used right after `insert`
    /// to deliver the mandatory first `SnapshotBaseline`, before the caller
    /// has any other event to fan out.
    pub fn send_to(&mut self, id: u64, build: impl FnOnce(u64) -> HarnessOperatorEventV1) {
        let Some(index) = self.subscribers.iter().position(|subscriber| subscriber.id == id)
        else {
            return;
        };
        match self.subscribers[index].try_send(build) {
            SubscriberSendOutcome::Sent => {}
            SubscriberSendOutcome::Full => { self.subscribers[index].needs_baseline = true; }
            SubscriberSendOutcome::Closed => self.remove_at(index),
        }
    }

    /// Fans an event out to every live, caught-up subscriber. `build` is
    /// called once per subscriber, not once total: every subscriber owns
    /// its own `sequence` counter (see `HarnessOperatorEventV1`'s doc
    /// comment), so the same logical event carries a different `sequence`
    /// per recipient. A subscriber already marked `needs_baseline` is
    /// skipped entirely -- it is treated as stale until `recover_lagged`
    /// catches it back up, so it must not observe this event out of order
    /// relative to the `SnapshotBaseline` it is waiting for.
    pub fn emit(&mut self, mut build: impl FnMut(u64) -> HarnessOperatorEventV1) {
        let mut index = 0;
        while index < self.subscribers.len() {
            if self.subscribers[index].needs_baseline {
                index += 1;
                continue;
            }
            match self.subscribers[index].try_send(&mut build) {
                SubscriberSendOutcome::Sent => index += 1,
                SubscriberSendOutcome::Full => {
                    let subscriber = &mut self.subscribers[index];
                    subscriber.needs_baseline = true;
                    tracing::info!(
                        subscriber_id = subscriber.id,
                        operation = %subscriber.identity.operation,
                        node_id = subscriber.identity.node_id(),
                        "harness operator event subscriber lagged: outbound queue is full",
                    );
                    index += 1;
                }
                SubscriberSendOutcome::Closed => self.remove_at(index),
            }
        }
    }

    /// Whether any live subscriber's outbound queue is currently full and
    /// awaiting a `Lagged`+`SnapshotBaseline` recovery pair --
    /// `recover_lagged_with`'s own cheap pre-check, promoted `pub` so a
    /// caller whose baseline is expensive (or, for `gate4agent-harness-light`,
    /// needs an async lock read no sync closure can perform) to assemble
    /// can decide whether to pay that cost at all before calling it.
    pub fn needs_recovery(&self) -> bool {
        self.subscribers.iter().any(|subscriber| subscriber.needs_baseline)
    }

    /// Runs once per select-loop pass. Every subscriber marked
    /// `needs_baseline` gets a `Lagged` frame followed by a fresh
    /// `SnapshotBaseline`; the flag clears only once both sends succeed --
    /// if either one is still `Full`, the subscriber stays marked and this
    /// retries on the next pass. `baseline` is called at most once, and only
    /// once `needs_recovery` is true (mirroring the inline check this was
    /// generalized from) -- the (potentially non-trivial, and for
    /// `gate4agent-harness-light`, async-lock-guarded) baseline assembly
    /// only ever runs when at least one subscriber actually needs it.
    ///
    /// Promoted `pub` and generalized from the original `recover_lagged`
    /// (kept below, now a thin wrapper) so `gate4agent-harness-light` (no
    /// `HarnessService`/`HarnessRuntimeInventoryCache`, no task/run kernel at
    /// all) can supply its own light-local baseline -- empty tasks/runs
    /// (canon: light has no kernel) and nodes from its own shared,
    /// `Arc<RwLock<..>>`-guarded runtime inventory -- while every other bit
    /// of overflow/pruning/retry bookkeeping stays exactly this one shared
    /// implementation.
    pub fn recover_lagged_with(
        &mut self,
        baseline: impl FnOnce() -> (Vec<RedactedTaskV1>, Vec<RedactedRunV1>, Vec<HarnessRuntimeNodeInventoryV1>),
    ) {
        if !self.needs_recovery() {
            return;
        }
        let (tasks, runs, nodes) = baseline();
        let mut index = 0;
        while index < self.subscribers.len() {
            if !self.subscribers[index].needs_baseline {
                index += 1;
                continue;
            }
            match self.subscribers[index]
                .try_send(|sequence| HarnessOperatorEventV1::Lagged { sequence })
            {
                SubscriberSendOutcome::Sent => {}
                SubscriberSendOutcome::Full => {
                    index += 1;
                    continue;
                }
                SubscriberSendOutcome::Closed => {
                    self.remove_at(index);
                    continue;
                }
            }
            let (tasks, runs, nodes) = (tasks.clone(), runs.clone(), nodes.clone());
            match self.subscribers[index].try_send(move |sequence| {
                HarnessOperatorEventV1::SnapshotBaseline { sequence, tasks, runs, nodes }
            }) {
                SubscriberSendOutcome::Sent => {
                    self.subscribers[index].needs_baseline = false;
                    index += 1;
                }
                SubscriberSendOutcome::Full => index += 1,
                SubscriberSendOutcome::Closed => self.remove_at(index),
            }
        }
    }

    /// This crate's own call site: `HarnessService`/`HarnessRuntimeInventoryCache`
    /// -backed baseline, over `recover_lagged_with`.
    fn recover_lagged(
        &mut self,
        harness: &HarnessService,
        runtime_inventory: &HarnessRuntimeInventoryCache,
    ) {
        self.recover_lagged_with(|| {
            let (tasks, runs) = harness_snapshot_baseline_payload(harness);
            let nodes = runtime_inventory.all_nodes();
            (tasks, runs, nodes)
        });
    }
}

/// Accumulates the task/run identities touched by a batch of engine
/// mutations that happen several call frames away from the code that
/// eventually needs to turn them into `HarnessOperatorEventV1::{TaskChanged,
/// RunChanged}` notifications -- the live-event and recovery-lifecycle
/// appliers (`apply_exact_control_lifecycle`, `apply_snapshot_lifecycle`,
/// `freeze_bound_route_waiting`, `reconcile_task_result_refs`). An id
/// landing here more than once is free: `notify_touched` re-reads current
/// engine state and redacts once per call, so a duplicate id is just a
/// duplicate, harmless wire event, never a duplicate expensive computation.
#[derive(Default)]
struct EngineTouch {
    task_ids: Vec<gate4agent_harness_protocol::HarnessTaskId>,
    run_ids: Vec<gate4agent_harness_protocol::HarnessRunId>,
}

impl EngineTouch {
    fn merge(&mut self, other: EngineTouch) {
        self.task_ids.extend(other.task_ids);
        self.run_ids.extend(other.run_ids);
    }
}

fn harness_snapshot_baseline_payload(
    harness: &HarnessService,
) -> (Vec<RedactedTaskV1>, Vec<RedactedRunV1>) {
    (
        harness.engine().tasks().map(redact_operator_task).collect(),
        harness.engine().runs().map(redact_operator_run).collect(),
    )
}

fn notify_task_changed(
    subscribers: &mut SubscriberRegistry,
    harness: &HarnessService,
    task_id: &gate4agent_harness_protocol::HarnessTaskId,
) {
    if subscribers.is_empty() { return; }
    let Some(task) = harness.engine().task(task_id) else { return; };
    let redacted = redact_operator_task(task);
    subscribers.emit(|sequence| HarnessOperatorEventV1::TaskChanged {
        sequence,
        task: redacted.clone(),
    });
}

fn notify_run_changed(
    subscribers: &mut SubscriberRegistry,
    harness: &HarnessService,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
) {
    if subscribers.is_empty() { return; }
    let Some(run) = harness.engine().run(run_id) else { return; };
    let redacted = redact_operator_run(run);
    subscribers.emit(|sequence| HarnessOperatorEventV1::RunChanged {
        sequence,
        run: redacted.clone(),
    });
}

fn notify_touched(subscribers: &mut SubscriberRegistry, harness: &HarnessService, touch: &EngineTouch) {
    if subscribers.is_empty() { return; }
    for task_id in &touch.task_ids { notify_task_changed(subscribers, harness, task_id); }
    for run_id in &touch.run_ids { notify_run_changed(subscribers, harness, run_id); }
}

/// Called once per `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL` tick (see that
/// constant's own doc comment for why this exists at all): pushes a `Ping`
/// to every live subscriber through the registry's ordinary `emit` path, so
/// a subscriber whose peer went away without any real event ever needing to
/// reach it gets reaped exactly the way a real event's failed write would
/// reap it -- `emit`'s existing `Closed` handling
/// (`SubscriberRegistry::remove_at`) already does the right thing here, and
/// already logs it; this function adds no logging of its own; a tick that
/// narrated itself on every fire would be worse than the leak it exists to
/// close. A healthy, idle subscriber simply receives one more frame it
/// drops on the floor (`Ping` carries no state to act on) -- `emit`'s
/// `Sent`/`Full` outcomes are unchanged, so this never disturbs a live
/// connection.
fn emit_subscriber_keepalive(subscribers: &mut SubscriberRegistry) {
    subscribers.emit(|sequence| HarnessOperatorEventV1::Ping { sequence });
}

/// Best-effort task/run change notification for a `HostCommand::*Finished`
/// dispatch-pipeline arm keyed by `operation_id`
/// (`DispatchPreflightFinished`/`DispatchFinished`/`HarnessMcpArmFinished`/
/// `DeliveryStageFinished`/`ContinuationExportFinished`, plus
/// `ApplyHarnessMutation` via `HarnessMutationV1::operation()`): every one of
/// them mutates exactly the run+task the operation already points at
/// (`HarnessOperationV1::task_id`/`run_id`), so re-reading the operation
/// after the mutation and redacting whatever it now names is enough --
/// cheaper and less fragile than threading a touch-accumulator through each
/// of those helpers individually. A harmless no-op re-send on a branch that
/// ended up not changing anything observable is tolerated: the TUI apply
/// path is an idempotent upsert.
fn notify_operation_touched(
    subscribers: &mut SubscriberRegistry,
    harness: &HarnessService,
    operation_id: &HarnessOperationId,
) {
    if subscribers.is_empty() { return; }
    let Some(operation) = harness.engine().operation(operation_id) else { return; };
    let task_id = operation.task_id.clone();
    let run_id = operation.run_id.clone();
    if let Some(task_id) = &task_id { notify_task_changed(subscribers, harness, task_id); }
    if let Some(run_id) = &run_id { notify_run_changed(subscribers, harness, run_id); }
}

/// The task id a successful operator mutation-family request touches, known
/// directly from the request body -- every mutation variant carries its
/// target `task_id`, including a freshly minted one for `CreateTask` (see
/// `HarnessCreateTaskRequestV1`). `SubmitIntent` is resolved exactly the way
/// `execute_operator_request` itself resolves it
/// (`authorize_operator_intent`, deterministic and pure) so the same task id
/// comes out whether the caller submitted the concrete request directly or
/// wrapped it in an intent. `None` for every read-only request and for
/// `ScheduleNext`, whose target task/run is only known from the response's
/// `HarnessDispatchIntentV1` (see `scheduled_dispatch_from_operator_response`,
/// used at the mutation-family reply site instead).
fn operator_mutation_task_id(
    request: &HarnessOperatorRequestV1,
) -> Option<gate4agent_harness_protocol::HarnessTaskId> {
    match request {
        HarnessOperatorRequestV1::CreateTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::ReplaceTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::MoveTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::CancelTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::RetryTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::ReplaceTaskExecutionSpec { request } => {
            Some(request.task_id.clone())
        }
        HarnessOperatorRequestV1::StartTask { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 { request } => {
            Some(request.task_id.clone())
        }
        HarnessOperatorRequestV1::StartTaskV2 { request } => Some(request.task_id.clone()),
        HarnessOperatorRequestV1::SubmitIntent { intent } => {
            authorize_operator_intent(intent.clone()).ok()
                .and_then(|resolved| operator_mutation_task_id(&resolved))
        }
        _ => None,
    }
}

type ObservationRecoveryRouteKey = (NodeId, NodeIncarnationId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservationRecoveryAttempt {
    attempt_id: u64,
    requested_after: u64,
}

#[derive(Debug)]
struct RouteObservationRecovery {
    route: NodeRoute,
    attempt: Option<ObservationRecoveryAttempt>,
    buffered: BTreeMap<u64, RoutedNodeEvent>,
    buffered_bytes: usize,
    overflowed: bool,
    refresh_after_completion: bool,
    retry_after: Instant,
    /// Sessions a caller has told this route to expect gone -- today, only
    /// a successful `StopSession` (see `HostCommand::SessionControlFinished`'s
    /// `stopped_session` field). A forced stop's C2 relay ack can outrace
    /// the node's own internal session-list update, so a resync that lands
    /// before the node has caught up is structurally valid but stale: as
    /// long as any of these are still present in the just-refreshed
    /// inventory, `finish_observation_recovery` keeps retrying (the same
    /// `retry_after` cadence an ordinary transport failure already uses)
    /// instead of accepting that resync as final. Cleared the moment none
    /// of them are present anymore.
    awaiting_absent_sessions: Vec<SessionAddress>,
}

impl RouteObservationRecovery {
    fn new(route: NodeRoute) -> Self {
        Self {
            route,
            attempt: None,
            buffered: BTreeMap::new(),
            buffered_bytes: 0,
            overflowed: false,
            refresh_after_completion: false,
            retry_after: Instant::now(),
            awaiting_absent_sessions: Vec::new(),
        }
    }

    fn accepts_completion(
        &self,
        route: &NodeRoute,
        attempt_id: u64,
        requested_after: u64,
    ) -> bool {
        self.route == *route
            && self.attempt == Some(ObservationRecoveryAttempt {
                attempt_id,
                requested_after,
            })
    }

    fn buffer(&mut self, routed: RoutedNodeEvent) {
        if self.overflowed || self.buffered.contains_key(&routed.cursor.sequence) {
            return;
        }
        let encoded_len = match serde_json::to_vec(&routed) {
            Ok(encoded) => encoded.len(),
            Err(_) => {
                self.buffered.clear();
                self.buffered_bytes = 0;
                self.overflowed = true;
                self.refresh_after_completion = true;
                return;
            }
        };
        if self.buffered.len() >= OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX
            || self.buffered_bytes.saturating_add(encoded_len)
                > OBSERVATION_RECOVERY_BUFFERED_BYTES_MAX
        {
            self.buffered.clear();
            self.buffered_bytes = 0;
            self.overflowed = true;
            self.refresh_after_completion = true;
            return;
        }
        self.buffered_bytes += encoded_len;
        self.buffered.insert(routed.cursor.sequence, routed);
    }

    fn prepare_follow_up(&mut self) {
        self.overflowed = false;
        self.refresh_after_completion = false;
        self.retry_after = Instant::now();
    }
}

#[derive(Debug, Default)]
struct ObservationRecoveryRegistry {
    routes: BTreeMap<ObservationRecoveryRouteKey, RouteObservationRecovery>,
    next_attempt_id: u64,
}

impl ObservationRecoveryRegistry {
    fn key(route: &NodeRoute) -> ObservationRecoveryRouteKey {
        (route.node_id.clone(), route.expected_incarnation_id)
    }

    fn ensure_route(&mut self, route: NodeRoute) -> &mut RouteObservationRecovery {
        self.routes.entry(Self::key(&route))
            .or_insert_with(|| RouteObservationRecovery::new(route))
    }

    fn contains(&self, route: &NodeRoute) -> bool {
        self.routes.contains_key(&Self::key(route))
    }

    fn remove(&mut self, route: &NodeRoute) {
        self.routes.remove(&Self::key(route));
    }

    fn reconcile_topology(&mut self, current: &[NodeRoute]) {
        self.routes.retain(|_, recovery| {
            current.iter().any(|route| route == &recovery.route)
        });
        for route in current {
            let recovery = self.ensure_route(route.clone());
            if recovery.attempt.is_some() {
                recovery.refresh_after_completion = true;
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.routes.values().filter(|recovery| recovery.attempt.is_some()).count()
    }

    fn allocate_attempt_id(&mut self) -> u64 {
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        self.next_attempt_id
    }
}

struct ActiveHarnessMcpActivation {
    attempt_id: u64,
    expected_revision: HarnessRevision,
    updated_at_unix_ms: u64,
    reply: Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveHarnessMcpRelay {
    attempt_id: u64,
}

#[derive(Default)]
struct HarnessMcpWorkerRegistry {
    next_attempt_id: u64,
    activations: BTreeMap<HarnessMcpReservationId, ActiveHarnessMcpActivation>,
    relays: BTreeMap<(HarnessMcpReservationId, HarnessMcpCallId), ActiveHarnessMcpRelay>,
}

#[derive(Default)]
struct NativeHistoryWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct RunReadWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct NodeWorkspaceReadWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct NodeWorkspaceWriteWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct SessionSpawnWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct SessionControlWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct SessionRecordMutationWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct HostDirectoryBrowseWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct ResourceMutationWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct RunGitFactsWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct RunContextSourceWorkerRegistry {
    in_flight: usize,
}

struct PendingRunContextSourceReply {
    prepared: PreparedRunContextSourceObservation,
    projection: RunContextSourceProjection,
    deadline: Instant,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
}

impl NativeHistoryWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= NATIVE_HISTORY_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl RunReadWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= RUN_READ_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl NodeWorkspaceReadWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= NODE_WORKSPACE_READ_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl NodeWorkspaceWriteWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= NODE_WORKSPACE_WRITE_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl SessionSpawnWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= SESSION_SPAWN_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl SessionControlWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= SESSION_CONTROL_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl SessionRecordMutationWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= SESSION_RECORD_MUTATION_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl HostDirectoryBrowseWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= HOST_DIRECTORY_BROWSE_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl ResourceMutationWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= RESOURCE_MUTATION_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl RunGitFactsWorkerRegistry {
    fn has_capacity(&self) -> bool {
        self.in_flight < RUN_GIT_FACTS_WORKERS_MAX
    }

    fn try_start(&mut self) -> bool {
        if self.in_flight >= RUN_GIT_FACTS_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl RunContextSourceWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= RUN_CONTEXT_SOURCE_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl HarnessMcpWorkerRegistry {
    fn in_flight(
        &self,
        pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    ) -> usize {
        self.activations.len()
            + self.relays.len()
            + pending_aborts.values().filter(|pending| pending.attempt_id.is_some()).count()
    }

    fn has_capacity(
        &self,
        pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    ) -> bool {
        self.in_flight(pending_aborts) < HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX
    }

    fn allocate_attempt_id(&mut self) -> u64 {
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        self.next_attempt_id
    }

    fn accepts_activation(
        &self,
        reservation_id: &HarnessMcpReservationId,
        attempt_id: u64,
        expected_revision: HarnessRevision,
    ) -> bool {
        self.activations.get(reservation_id).is_some_and(|active| {
            active.attempt_id == attempt_id
                && active.expected_revision == expected_revision
        })
    }

    fn accepts_relay(
        &self,
        reservation_id: &HarnessMcpReservationId,
        call_id: &HarnessMcpCallId,
        attempt_id: u64,
    ) -> bool {
        self.relays.get(&(reservation_id.clone(), call_id.clone()))
            == Some(&ActiveHarnessMcpRelay { attempt_id })
    }
}

enum CoordinatorSpawnResult {
    Accepted(AcceptedSpawnBindingProof),
    Rejected(NodeFailureCode),
    Failed,
    /// Carries WHY the outcome is unknown, when the caller has it (`None`
    /// at the handful of call sites -- stranded-dispatch recovery at
    /// startup, an already-discarded downstream error -- that never had a
    /// specific cause to name). `apply_spawn_result` logs it; nothing else
    /// reads it.
    OutcomeUnknown(Option<String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorPreDispatchResult {
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContinuationResumeAction {
    BeginExport,
    RecoverOutcomeUnknown,
    Preflight,
    FinishOutcomeUnknown,
    FinishFailed,
    Reject,
}

fn continuation_resume_action(
    state: gate4agent_harness_protocol::HarnessContinuationStateV1,
) -> ContinuationResumeAction {
    match state {
        gate4agent_harness_protocol::HarnessContinuationStateV1::Prepared => {
            ContinuationResumeAction::BeginExport
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Exporting => {
            ContinuationResumeAction::RecoverOutcomeUnknown
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Exported => {
            ContinuationResumeAction::Preflight
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::OutcomeUnknown => {
            ContinuationResumeAction::FinishOutcomeUnknown
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Expired => {
            ContinuationResumeAction::FinishFailed
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Bound => {
            ContinuationResumeAction::Reject
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorDispatchPhase {
    Delivery,
    Continuation,
    Preflight,
    HarnessMcpArm,
    Spawn,
}

enum CoordinatorPreflightStart {
    Spawn {
        route: NodeRoute,
        pending: PendingCoordinatorSpawn,
        plan: crate::dispatch::HarnessLaunchPlanV1,
    },
    HarnessMcpArm {
        plan: crate::dispatch::HarnessLaunchPlanV1,
    },
}

enum PendingCoordinatorSpawn {
    Direct(crate::c2::PendingSpawnDispatch),
    Managed(PendingManagedWorktreeSpawnDispatch),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptedSpawnTransition {
    Plain,
    Delivery,
    Continuation,
    DeliveryAndContinuation,
    HarnessMcp,
    HarnessMcpDelivery,
    HarnessMcpContinuation,
    HarnessMcpDeliveryAndContinuation,
}

fn accepted_spawn_transition(
    plan: &crate::dispatch::HarnessLaunchPlanV1,
    issued_operator_transfer_authority: bool,
    has_delivery: bool,
    has_continuation: bool,
) -> Result<AcceptedSpawnTransition, HarnessRuntimeError> {
    let issued_ordinary = plan.is_ordinary_dispatch()
        && matches!(plan.grant, crate::dispatch::HarnessGrantPolicyV1::Operator)
        && issued_operator_transfer_authority;
    // Two independent reasons an accepted spawn may not match its launch
    // plan once the operator-issuance exception is out of the way: checked
    // one at a time so the refusal names which one fired instead of
    // collapsing delivery and continuation mismatches into one disjunction.
    if !issued_ordinary {
        if plan.delivery.is_some() != has_delivery {
            return Err(HarnessRuntimeError::DispatchPreparation(
                "accepted spawn's delivery presence does not match the launch plan",
            ));
        }
        if (plan.continuation == crate::dispatch::HarnessContinuationPolicyV1::ParentRun)
            != has_continuation
        {
            return Err(HarnessRuntimeError::DispatchPreparation(
                "accepted spawn's continuation presence does not match the launch plan's continuation policy",
            ));
        }
    }
    let harness_mcp = plan.harness_mcp == crate::dispatch::HarnessMcpPolicyV1::GrantBound;
    Ok(match (harness_mcp, has_delivery, has_continuation) {
        (false, false, false) => AcceptedSpawnTransition::Plain,
        (false, true, false) => AcceptedSpawnTransition::Delivery,
        (false, false, true) => AcceptedSpawnTransition::Continuation,
        (false, true, true) => AcceptedSpawnTransition::DeliveryAndContinuation,
        (true, false, false) => AcceptedSpawnTransition::HarnessMcp,
        (true, true, false) => AcceptedSpawnTransition::HarnessMcpDelivery,
        (true, false, true) => AcceptedSpawnTransition::HarnessMcpContinuation,
        (true, true, true) => AcceptedSpawnTransition::HarnessMcpDeliveryAndContinuation,
    })
}

fn has_issued_operator_transfer_authority(
    delivery: Option<&HarnessDeliveryV1>,
    continuation: Option<&HarnessContinuationV1>,
) -> bool {
    let mut issuance = None;
    for authority in [
        delivery.map(|delivery| &delivery.authority),
        continuation.map(|continuation| &continuation.authority),
    ].into_iter().flatten() {
        let HarnessTransferAuthorityRefV1::OperatorIssuance {
            issuance: candidate,
        } = authority else {
            return false;
        };
        if let Some(expected) = issuance {
            if expected != candidate {
                return false;
            }
        }
        issuance = Some(candidate);
    }
    issuance.is_some()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveDispatchJob {
    operation_id: HarnessOperationId,
    phase: CoordinatorDispatchPhase,
    /// The exact launch plan resolved when this job started (or last
    /// resumed) -- carried through every later phase of the SAME dispatch
    /// so a phase-completion handler (`DispatchFinished` -> `apply_spawn_
    /// result`'s `Accepted` arm, specifically) never needs to re-derive it
    /// from `effective_launch_catalog` at that later moment. Re-deriving
    /// there raced a live `HarnessRuntimeInventoryCache` invalidation: the
    /// Control event that confirms the very spawn this job is completing
    /// is itself one of the events that invalidates the target node's
    /// cached inventory (`event_affects_runtime_inventory`) until the next
    /// resync lands, and a derived plan resolves to nothing while its node
    /// is invalidated. A CLI plan is immune (its catalog is static and
    /// never invalidated), which is why this only ever surfaced for a
    /// derived one.
    plan: crate::dispatch::HarnessLaunchPlanV1,
}

impl ActiveDispatchJob {
    fn new(
        operation_id: HarnessOperationId,
        phase: CoordinatorDispatchPhase,
        plan: crate::dispatch::HarnessLaunchPlanV1,
    ) -> Self {
        Self { operation_id, phase, plan }
    }

    fn is(&self, operation_id: &HarnessOperationId, phase: CoordinatorDispatchPhase) -> bool {
        &self.operation_id == operation_id && self.phase == phase
    }
}

fn next_runtime_revision(revision: HarnessRevision) -> Result<HarnessRevision, HarnessRuntimeError> {
    HarnessRevision::new(
        revision.get().checked_add(1).ok_or(HarnessRuntimeError::DispatchPreparation(
            "harness revision counter overflowed u64",
        ))?,
    ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
        "incremented harness revision is zero",
    ))
}

fn specialized_spawn_spec(
    harness: &HarnessService,
    _plan: &crate::dispatch::HarnessLaunchPlanV1,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    mut spec: gate4agent_node_protocol::SpawnSpec,
) -> Result<gate4agent_node_protocol::SpawnSpec, HarnessRuntimeError> {
    if let Some(delivery) = harness.engine().delivery_for_run(run_id) {
        let stage = delivery.stage_receipt.as_ref()
            .ok_or(HarnessRuntimeError::DispatchPreparation(
                "delivery has no stage receipt",
            ))?;
        spec.overrides.bundle_id = gate4agent_node_protocol::SpawnOverride::Set {
            value: SpawnBundleId::new(stage.bundle.bundle_id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "stage receipt bundle id is not a valid spawn bundle id",
                ))?,
        };
    }
    if let Some(continuation) = harness.engine().continuation_for_run(run_id) {
        let context = continuation.context.as_ref()
            .ok_or(HarnessRuntimeError::DispatchPreparation(
                "continuation has no context",
            ))?;
        spec.overrides.context_id = gate4agent_node_protocol::SpawnOverride::Set {
            value: SpawnContextId::new(context.id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "continuation context id is not a valid spawn context id",
                ))?,
        };
    }
    Ok(spec)
}

/// Reactive, cheap, in-memory-only reconciliation of every task's
/// `result_refs` index against its own already-durable runs — a run's
/// `result_disposition` is committed by the caller before this runs, so a
/// redundant scan costs nothing correctness-relevant
/// (`record_task_result_ref`'s own (task_id, ref) idempotency makes a
/// repeat insertion attempt a free `Replayed` no-op, same posture as
/// `apply_context_pack_receipt_for_record`). Safe to call unconditionally
/// after any commit that could have newly set a run's `result_disposition`
/// — wired at the three sites the A3 design §5.1 names. Also the whole
/// reason restart-survival is free (§5.3): this is a pure function of
/// already-durable state (`run.result_disposition`, reloaded from
/// `harness_runs.payload`), so a call after any future commit re-derives
/// the same fixed point regardless of what the in-memory host state looked
/// like before the restart.
fn reconcile_task_result_refs(
    harness: &mut HarnessService,
    now_unix_ms: u64,
) -> Result<Vec<gate4agent_harness_protocol::HarnessTaskId>, HarnessRuntimeError> {
    let mut matches = Vec::new();
    for task in harness.engine().tasks() {
        for run_id in &task.run_ids {
            let Some(run) = harness.engine().run(run_id) else { continue; };
            if run.result_disposition.is_none() {
                continue;
            }
            let result_ref = HarnessResultRef::for_run(&run.run_id);
            if task.result_refs.binary_search(&result_ref).is_err() {
                matches.push((task.task_id.clone(), run.run_id.clone()));
            }
        }
    }
    let mut touched_task_ids = Vec::with_capacity(matches.len());
    for (task_id, run_id) in matches {
        harness.record_task_result_ref(&task_id, &run_id, now_unix_ms)
            .map_err(HarnessRuntimeError::Harness)?;
        touched_task_ids.push(task_id);
    }
    Ok(touched_task_ids)
}

fn apply_spawn_result(
    harness: &mut HarnessService,
    launch_catalog: &HarnessLaunchCatalog,
    operation_id: &HarnessOperationId,
    result: CoordinatorSpawnResult,
    now_unix_ms: u64,
) -> Result<Option<HarnessMcpReservationId>, HarnessRuntimeError> {
    let operation = harness.engine().operation(operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation is missing"))?.clone();
    let run_id = operation.run_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation carries no run id"))?;
    let task_id = operation.task_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation carries no task id"))?;
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("run is missing"))?.clone();
    let task = harness.engine().task(task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("task is missing"))?.clone();
    // Three independent reasons a completing spawn may no longer match the
    // state this function expects: checked one at a time so the refusal
    // names which one fired instead of collapsing them into "state
    // mismatch".
    if run.lifecycle != HarnessRunLifecycleV1::Dispatching {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "run is not in the Dispatching lifecycle",
        ));
    }
    if operation.state != HarnessOperationStateV1::Dispatching {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "operation is not in the Dispatching state",
        ));
    }
    if task.state != HarnessTaskStateV1::Running {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "task is not in the Running state",
        ));
    }

    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = now_unix_ms;
    let mut next_operation = operation.clone();
    next_operation.revision = next_runtime_revision(operation.revision)?;
    next_operation.updated_at_unix_ms = now_unix_ms;
    match result {
        CoordinatorSpawnResult::Accepted(proof) => {
            let (instance_id, generation) = proof.runtime_identity();
            next_run.lifecycle = HarnessRunLifecycleV1::Running;
            next_run.binding = Some(HarnessSessionBindingV1 {
                node_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.node_id().as_str(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "spawn proof node id is not a valid harness selector",
                ))?,
                node_incarnation: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.incarnation_id().to_string(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "spawn proof incarnation id is not a valid harness selector",
                ))?,
                workspace_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.workspace_id().as_str(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "spawn proof workspace id is not a valid harness selector",
                ))?,
                session: HarnessSessionIdentityV1::Managed {
                    record_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                        proof.record_id().as_str(),
                    ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                        "spawn proof record id is not a valid harness selector",
                    ))?,
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id,
                        generation,
                    }),
                },
            });
            next_operation.state = HarnessOperationStateV1::Succeeded;
            next_operation.finished_at_unix_ms = Some(now_unix_ms);
            let scheduled = harness.scheduled_launch(operation_id)
                .ok_or(HarnessRuntimeError::DispatchPreparation(
                    "operation has no scheduled launch",
                ))?;
            let plan = launch_catalog.resolve_scheduled(scheduled)
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "scheduled launch does not resolve against the launch catalog",
                ))?;
            let delivery = harness.engine().delivery_for_run(run_id).cloned();
            let continuation = harness.engine().continuation_for_run(run_id).cloned();
            let has_delivery = delivery.is_some();
            let has_continuation = continuation.is_some();
            let transition = accepted_spawn_transition(
                plan,
                has_issued_operator_transfer_authority(
                    delivery.as_ref(),
                    continuation.as_ref(),
                ),
                has_delivery,
                has_continuation,
            )?;
            let ids = deterministic_issued_dispatch_ids(
                operation_id,
                has_delivery,
                has_continuation,
            ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                "operation id fails deterministic dispatch id derivation",
            ))?;
            if let Some(continuation) = &continuation {
                next_run.continuation_receipt = Some(continuation.receipt_ref.clone());
            }
            let committed_delivery = if let Some(mut delivery) = delivery {
                let receipt_ref = ids.delivery_receipt_ref
                    .ok_or(HarnessRuntimeError::DispatchPreparation(
                        "dispatch ids carry no delivery receipt ref",
                    ))?;
                next_run.delivery_receipt = Some(receipt_ref.clone());
                let binding = next_run.binding.clone()
                    .ok_or(HarnessRuntimeError::DispatchPreparation(
                        "run carries no session binding",
                    ))?;
                let expected_delivery_revision = delivery.revision;
                delivery.revision = next_runtime_revision(delivery.revision)?;
                delivery.state = gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed;
                delivery.receipt = Some(crate::delivery::terminal_receipt(
                    &delivery,
                    receipt_ref,
                    binding,
                    now_unix_ms,
                )?);
                delivery.updated_at_unix_ms = now_unix_ms;
                Some((expected_delivery_revision, delivery))
            } else {
                None
            };
            match transition {
                AcceptedSpawnTransition::Plain => harness.transition_run_with_accepted_spawn(
                    run.revision,
                    next_run,
                    operation.revision,
                    next_operation,
                    &proof,
                ).map(|()| None).map_err(HarnessRuntimeError::Harness),
                AcceptedSpawnTransition::Delivery => {
                    let (expected_delivery_revision, delivery) = committed_delivery
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "committed delivery is missing for the Delivery transition",
                        ))?;
                    harness.transition_run_with_accepted_spawn_and_delivery(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        expected_delivery_revision,
                        delivery,
                        &proof,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::Continuation => {
                    let continuation = continuation.as_ref()
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "continuation is missing for the Continuation transition",
                        ))?;
                    harness.transition_run_with_accepted_spawn_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &continuation.continuation_ref,
                        continuation.revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::DeliveryAndContinuation => {
                    let (expected_delivery_revision, delivery) = committed_delivery
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "committed delivery is missing for the DeliveryAndContinuation transition",
                        ))?;
                    let continuation = continuation.as_ref()
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "continuation is missing for the DeliveryAndContinuation transition",
                        ))?;
                    harness.transition_run_with_accepted_spawn_delivery_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        expected_delivery_revision,
                        delivery,
                        &continuation.continuation_ref,
                        continuation.revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::HarnessMcp => {
                    harness.transition_run_with_accepted_harness_mcp_spawn(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::HarnessMcpDelivery => {
                    let (expected_delivery_revision, delivery) = committed_delivery
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "committed delivery is missing for the HarnessMcpDelivery transition",
                        ))?;
                    harness.transition_run_with_accepted_harness_mcp_spawn_and_delivery(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        expected_delivery_revision,
                        delivery,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::HarnessMcpContinuation => {
                    let continuation = continuation.as_ref()
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "continuation is missing for the HarnessMcpContinuation transition",
                        ))?;
                    harness.transition_run_with_accepted_harness_mcp_spawn_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &continuation.continuation_ref,
                        continuation.revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
                AcceptedSpawnTransition::HarnessMcpDeliveryAndContinuation => {
                    let (expected_delivery_revision, delivery) = committed_delivery
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "committed delivery is missing for the HarnessMcpDeliveryAndContinuation transition",
                        ))?;
                    let continuation = continuation.as_ref()
                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                            "continuation is missing for the HarnessMcpDeliveryAndContinuation transition",
                        ))?;
                    harness.transition_run_with_accepted_harness_mcp_spawn_delivery_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        expected_delivery_revision,
                        delivery,
                        &continuation.continuation_ref,
                        continuation.revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
            }
        }
        CoordinatorSpawnResult::Rejected(code) => {
            // `HarnessFailureV1` carries only the category, so the node's
            // own code is the sole record of WHY the spawn was refused --
            // dropping it leaves a terminal Failed run whose cause exists
            // nowhere. Named here, at the transition that makes it
            // terminal.
            tracing::warn!(
                operation = %operation_id,
                run = %run_id,
                task = %task_id,
                node_failure_code = ?code,
                "node rejected the spawn: run and task go terminal Failed",
            );
            apply_rejected_spawn(
                harness,
                &run,
                next_run,
                &operation,
                next_operation,
                &task,
                now_unix_ms,
            )
        }
        CoordinatorSpawnResult::Failed => {
            tracing::warn!(
                operation = %operation_id,
                run = %run_id,
                task = %task_id,
                "spawn dispatch failed before the node answered: run and task go terminal Failed",
            );
            apply_rejected_spawn(
                harness,
                &run,
                next_run,
                &operation,
                next_operation,
                &task,
                now_unix_ms,
            )
        }
        CoordinatorSpawnResult::OutcomeUnknown(reason) => {
            // Unlike `Rejected`/`Failed` above, this used to commit
            // `ReplyLost` with no log line anywhere -- the operator saw a
            // stuck run and had nothing to grep for. Name the cause here,
            // at the transition that makes it retryable.
            tracing::warn!(
                operation = %operation_id,
                run = %run_id,
                task = %task_id,
                reason = reason.as_deref().unwrap_or("cause not captured"),
                "spawn outcome is unknown after a transport/protocol failure: run and task go retryable OutcomeUnknown",
            );
            next_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
            next_operation.state = HarnessOperationStateV1::OutcomeUnknown;
            next_operation.outcome_unknown_reason = Some(
                HarnessOutcomeUnknownReasonV1::ReplyLost,
            );
            let mut next_task = task.clone();
            next_task.revision = next_runtime_revision(task.revision)?;
            next_task.state = HarnessTaskStateV1::Waiting;
            next_task.updated_at_unix_ms = now_unix_ms;
            harness.commit_scheduled_pre_dispatch_outcome(
                run.revision,
                next_run,
                operation.revision,
                next_operation,
                task.revision,
                next_task,
            ).map_err(HarnessRuntimeError::Harness)
        }
    }
}

/// Commits the terminal-Failed outcome both refusal shapes share: the
/// node answered with a rejection code, or the dispatch never got an
/// answer worth trusting. Both land on the same `Rejected`/non-retryable
/// `HarnessFailureV1` — the distinction between them lives in the caller's
/// own log line, which is where the node's code is named, not in the
/// state written here.
fn apply_rejected_spawn(
    harness: &mut HarnessService,
    run: &HarnessRunV1,
    mut next_run: HarnessRunV1,
    operation: &HarnessOperationV1,
    mut next_operation: HarnessOperationV1,
    task: &HarnessTaskV1,
    now_unix_ms: u64,
) -> Result<Option<HarnessMcpReservationId>, HarnessRuntimeError> {
    let failure = HarnessFailureV1 {
        category: HarnessFailureCategoryV1::Rejected,
        retryable: false,
    };
    next_run.lifecycle = HarnessRunLifecycleV1::Failed;
    next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
    next_run.failure = Some(failure.clone());
    next_operation.state = HarnessOperationStateV1::Failed;
    next_operation.failure = Some(failure);
    next_operation.finished_at_unix_ms = Some(now_unix_ms);
    let mut next_task = task.clone();
    next_task.revision = next_runtime_revision(task.revision)?;
    next_task.state = HarnessTaskStateV1::Failed;
    next_task.updated_at_unix_ms = now_unix_ms;
    let reservation = harness.commit_scheduled_pre_dispatch_outcome(
        run.revision,
        next_run,
        operation.revision,
        next_operation,
        task.revision,
        next_task,
    ).map_err(HarnessRuntimeError::Harness)?;
    reconcile_task_result_refs(harness, now_unix_ms)?;
    Ok(reservation)
}

fn apply_pre_dispatch_result(
    harness: &mut HarnessService,
    operation_id: &HarnessOperationId,
    result: CoordinatorPreDispatchResult,
    now_unix_ms: u64,
) -> Result<Option<HarnessMcpReservationId>, HarnessRuntimeError> {
    let operation = harness.engine().operation(operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation is missing"))?
        .clone();
    let run_id = operation.run_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation carries no run id"))?;
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("run is missing"))?
        .clone();
    let task = harness.engine().task(&run.task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("task is missing"))?
        .clone();
    // Three independent reasons a pre-dispatch outcome may no longer match
    // the state this function expects: checked one at a time so the
    // refusal names which one fired instead of collapsing them into "state
    // mismatch".
    if run.lifecycle != HarnessRunLifecycleV1::Requested {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "run is not in the Requested lifecycle",
        ));
    }
    if operation.state != HarnessOperationStateV1::Prepared {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "operation is not in the Prepared state",
        ));
    }
    if task.state != HarnessTaskStateV1::Running {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "task is not in the Running state",
        ));
    }
    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = now_unix_ms;
    let mut next_operation = operation.clone();
    next_operation.revision = next_runtime_revision(operation.revision)?;
    next_operation.updated_at_unix_ms = now_unix_ms;
    let mut next_task = task.clone();
    next_task.revision = next_runtime_revision(task.revision)?;
    next_task.updated_at_unix_ms = now_unix_ms;
    match result {
        CoordinatorPreDispatchResult::Failed => {
            // Not every path here comes through one of the classifiers
            // that name the underlying error, so this is the one line
            // guaranteed to mark WHICH run went terminal.
            tracing::warn!(
                operation = %operation_id,
                run = %run.run_id,
                task = %task.task_id,
                "pre-dispatch outcome is terminal: run and task go Failed/Rejected",
            );
            let failure = HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Rejected,
                retryable: false,
            };
            next_run.lifecycle = HarnessRunLifecycleV1::Failed;
            next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
            next_run.failure = Some(failure.clone());
            next_operation.state = HarnessOperationStateV1::Failed;
            next_operation.failure = Some(failure);
            next_operation.finished_at_unix_ms = Some(now_unix_ms);
            next_task.state = HarnessTaskStateV1::Failed;
        }
        CoordinatorPreDispatchResult::OutcomeUnknown => {
            next_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
            next_operation.state = HarnessOperationStateV1::OutcomeUnknown;
            next_operation.outcome_unknown_reason = Some(
                HarnessOutcomeUnknownReasonV1::ReplyLost,
            );
            next_task.state = HarnessTaskStateV1::Waiting;
        }
    }
    let reservation = harness.commit_scheduled_pre_dispatch_outcome(
        run.revision,
        next_run,
        operation.revision,
        next_operation,
        task.revision,
        next_task,
    ).map_err(HarnessRuntimeError::Harness)?;
    reconcile_task_result_refs(harness, now_unix_ms)?;
    Ok(reservation)
}

/// Resolves the `(grant_id, grant_revision)` a harness-MCP-bound dispatch
/// arms its H3B reservation against (gate4agent-arc-mailbox-and-task-layer
/// Slice A(i)). An `Exact` launch plan grant is used as given, unchanged.
/// An `Operator` plan grant names no exact grant to bind to, so this mints
/// the dispatching run's own default, read-only grant
/// (`SessionGrantV1::default_for_run`) through `HarnessService::apply` --
/// the identical mutation path the operator wire's own
/// `ApplyHarnessMutation` uses, so it gets the same validation and audit
/// trail -- and returns that grant's freshly-minted identity instead. The
/// grant's identity is derived deterministically from `dispatch_operation_id`
/// alone, so a retried dispatch mints (or replays) the identical grant
/// rather than a second one.
///
/// `pub(crate)` rather than private: `lib.rs`'s own test module composes
/// this with `HarnessService::begin_run_dispatch_with_harness_mcp` to
/// exercise the full mint-then-dispatch seam end to end (see
/// `h3b_dispatch_accepts_a_grant_minted_by_resolve_harness_mcp_grant`).
pub(crate) fn resolve_harness_mcp_grant(
    harness: &mut HarnessService,
    dispatch_operation_id: &HarnessOperationId,
    grant_policy: &crate::dispatch::HarnessGrantPolicyV1,
    actor_run_id: &gate4agent_harness_protocol::HarnessRunId,
    target: HarnessGrantTargetV1,
    now_unix_ms: u64,
) -> Result<(gate4agent_harness_protocol::SessionGrantId, HarnessRevision), HarnessRuntimeError> {
    match grant_policy {
        crate::dispatch::HarnessGrantPolicyV1::Exact { grant_id, revision } => {
            Ok((grant_id.clone(), *revision))
        }
        crate::dispatch::HarnessGrantPolicyV1::Operator => {
            let first_revision = HarnessRevision::new(1)
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "literal revision 1 is not a valid harness revision",
                ))?;
            let grant_ids = deterministic_default_grant_ids(dispatch_operation_id)
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "dispatch operation id fails deterministic default grant id derivation",
                ))?;
            let minted_grant_id = grant_ids.grant_id.clone();
            let default_grant = SessionGrantV1::default_for_run(
                grant_ids.grant_id,
                actor_run_id.clone(),
                target,
                now_unix_ms,
            );
            let placeholder_digest = HarnessRequestDigest::new("0".repeat(64))
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "literal all-zero request digest is not valid hex",
                ))?;
            let grant_operation = HarnessOperationV1 {
                operation_id: grant_ids.operation_id,
                revision: first_revision,
                actor: HarnessActorV1::ParentRun { run_id: actor_run_id.clone() },
                kind: HarnessOperationKindV1::CreateGrant,
                state: HarnessOperationStateV1::Succeeded,
                task_id: None,
                run_id: None,
                grant_id: Some(minted_grant_id.clone()),
                reconciles_operation_id: None,
                expected_revision: None,
                request_digest: placeholder_digest,
                idempotency_ref: grant_ids.idempotency_ref,
                failure: None,
                outcome_unknown_reason: None,
                reconciliation_outcome: None,
                created_at_unix_ms: now_unix_ms,
                updated_at_unix_ms: now_unix_ms,
                dispatched_at_unix_ms: None,
                finished_at_unix_ms: Some(now_unix_ms),
            };
            let mut mutation = HarnessMutationV1::CreateGrant {
                operation: grant_operation,
                grant: default_grant,
            };
            mutation.operation_mut().request_digest = mutation_request_digest(&mutation)
                .map_err(HarnessRuntimeError::Harness)?;
            harness.apply(mutation).map_err(HarnessRuntimeError::Harness)?;
            Ok((minted_grant_id, first_revision))
        }
    }
}

/// The four classifiers below are the choke point where one pre-dispatch
/// error becomes either a retryable `OutcomeUnknown` or a terminal
/// `Failed`. `CoordinatorPreDispatchResult` carries no payload and
/// `HarnessFailureV1` records only a category, so the error itself
/// survives nowhere past this decision -- a terminal run would otherwise
/// report "Rejected" with its cause existing in no log, no wire field and
/// no store row. Named here, where the decision is made.
fn note_terminal_pre_dispatch(stage: &str, error: &dyn std::fmt::Debug) {
    tracing::warn!(
        stage,
        error = ?error,
        "pre-dispatch error is terminal: the run and its task go Failed/Rejected",
    );
}

fn delivery_pre_dispatch_result(error: &HarnessC2Error) -> CoordinatorPreDispatchResult {
    if matches!(error, HarnessC2Error::DeliveryTransport(_)) {
        CoordinatorPreDispatchResult::OutcomeUnknown
    } else {
        note_terminal_pre_dispatch("delivery", error);
        CoordinatorPreDispatchResult::Failed
    }
}

fn delivery_stage_completion_result(
    error: &HarnessRuntimeError,
) -> CoordinatorPreDispatchResult {
    if matches!(
        error,
        HarnessRuntimeError::Harness(HarnessServiceError::InvalidStagedDeliveryProof(_))
            | HarnessRuntimeError::C2(HarnessC2Error::DeliveryTransport(_))
    ) {
        CoordinatorPreDispatchResult::OutcomeUnknown
    } else {
        note_terminal_pre_dispatch("delivery-stage-completion", error);
        CoordinatorPreDispatchResult::Failed
    }
}

fn preflight_pre_dispatch_result(error: &HarnessC2Error) -> CoordinatorPreDispatchResult {
    note_terminal_pre_dispatch("preflight", error);
    CoordinatorPreDispatchResult::Failed
}

fn dispatching_start_error_result(error: &HarnessRuntimeError) -> CoordinatorSpawnResult {
    match error {
        HarnessRuntimeError::C2(error) if error.start_failure_category().is_some() => {
            CoordinatorSpawnResult::Failed
        }
        _ => CoordinatorSpawnResult::OutcomeUnknown(Some(error.to_string())),
    }
}

/// Classifies the error `PendingHarnessMcpArm::finish` returned once the
/// Arm request's fate is known. A node rejection or a protocol violation
/// (an unnegotiated capability, a route/incarnation mismatch, an invalid
/// reply) is a certain, non-retryable outcome -- `Failed`. Everything else
/// -- the C2 control connection was lost, or the reply simply timed out --
/// is a genuine "the round trip's outcome was never learned", so it stays
/// retryable `OutcomeUnknown`, carrying the underlying error's own text as
/// its cause.
fn harness_mcp_arm_finish_result(error: &HarnessC2Error) -> CoordinatorSpawnResult {
    match error {
        HarnessC2Error::HarnessMcpRejected { .. } => CoordinatorSpawnResult::Failed,
        HarnessC2Error::HarnessMcpTransport(gate4agent_c2_client::C2ControlError::Protocol(_)) => {
            CoordinatorSpawnResult::Failed
        }
        _ => CoordinatorSpawnResult::OutcomeUnknown(Some(error.to_string())),
    }
}

fn delivery_needs_staging(
    state: gate4agent_harness_protocol::HarnessDeliveryStateV1,
) -> Result<bool, HarnessRuntimeError> {
    match state {
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Prepared => Ok(true),
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Staged => Ok(false),
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed => {
            Err(HarnessRuntimeError::DispatchPreparation("delivery is already Committed"))
        }
    }
}

fn commit_lifecycle_projection(
    harness: &mut HarnessService,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    node_id: &NodeId,
    incarnation_id: NodeIncarnationId,
    event_sequence: u64,
    kind: HarnessLifecycleEventKindV1,
    projection: HarnessLifecycleProjectionV1,
    now_unix_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("run is missing"))?.clone();
    if !matches!(run.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting) {
        return Ok(());
    }
    let task = harness.engine().task(&run.task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("task is missing"))?.clone();
    let projected_run_lifecycle = match projection {
        HarnessLifecycleProjectionV1::Running => HarnessRunLifecycleV1::Running,
        HarnessLifecycleProjectionV1::Waiting => HarnessRunLifecycleV1::Waiting,
        HarnessLifecycleProjectionV1::CompletedReview => HarnessRunLifecycleV1::Completed,
        HarnessLifecycleProjectionV1::Failed => HarnessRunLifecycleV1::Failed,
        HarnessLifecycleProjectionV1::Cancelled => HarnessRunLifecycleV1::Cancelled,
    };
    let projected_task_state = match projection {
        HarnessLifecycleProjectionV1::Running => HarnessTaskStateV1::Running,
        HarnessLifecycleProjectionV1::Waiting => HarnessTaskStateV1::Waiting,
        HarnessLifecycleProjectionV1::CompletedReview => HarnessTaskStateV1::Review,
        HarnessLifecycleProjectionV1::Failed => HarnessTaskStateV1::Failed,
        HarnessLifecycleProjectionV1::Cancelled => HarnessTaskStateV1::Cancelled,
    };
    if run.lifecycle == projected_run_lifecycle && task.state == projected_task_state {
        return Ok(());
    }
    let ids = deterministic_lifecycle_authority_ids(
        &run.run_id,
        node_id,
        &incarnation_id,
        event_sequence,
        kind,
    ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
        "lifecycle authority ids failed to derive for run",
    ))?;
    if harness.engine().operation(&ids.operation_id).is_some() {
        return Ok(());
    }
    let committed_at = now_unix_ms.max(run.updated_at_unix_ms).max(task.updated_at_unix_ms);
    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = committed_at;
    next_run.lifecycle = projected_run_lifecycle;
    match projection {
        HarnessLifecycleProjectionV1::CompletedReview => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Succeeded);
        }
        HarnessLifecycleProjectionV1::Failed => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
            next_run.failure = Some(HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Internal,
                retryable: false,
            });
        }
        HarnessLifecycleProjectionV1::Cancelled => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Cancelled);
        }
        HarnessLifecycleProjectionV1::Running | HarnessLifecycleProjectionV1::Waiting => {}
    }
    let mut next_task = task.clone();
    next_task.revision = next_runtime_revision(task.revision)?;
    next_task.updated_at_unix_ms = committed_at;
    next_task.state = projected_task_state;
    let operation = HarnessOperationV1 {
        operation_id: ids.operation_id,
        revision: HarnessRevision::new(1)
            .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                "new operation revision is zero",
            ))?,
        actor: HarnessActorV1::ParentRun { run_id: run.run_id.clone() },
        kind: HarnessOperationKindV1::MutateRun,
        state: HarnessOperationStateV1::Succeeded,
        task_id: None,
        run_id: Some(run.run_id.clone()),
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision: Some(run.revision),
        request_digest: ids.request_digest,
        idempotency_ref: ids.idempotency_ref,
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: committed_at,
        updated_at_unix_ms: committed_at,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(committed_at),
    };
    harness.commit_run_event(
        operation,
        run.revision,
        next_run,
        task.revision,
        next_task,
    ).map_err(HarnessRuntimeError::Harness)
}

/// Returns every task/run id this call actually committed a lifecycle
/// projection for, so a caller several frames away from the mutation
/// (`apply_or_buffer_host_live_event`, `finish_observation_recovery`) can
/// turn it into `TaskChanged`/`RunChanged` notifications without re-deriving
/// which run matched.
fn apply_exact_control_lifecycle(
    harness: &mut HarnessService,
    routed: &RoutedNodeEvent,
    now_unix_ms: u64,
) -> Result<EngineTouch, HarnessRuntimeError> {
    let matches = harness.engine().runs().filter_map(|run| {
        exact_bound_control_lifecycle(run, routed).map(|(sequence, kind, projection)| {
            (run.run_id.clone(), run.task_id.clone(), sequence, kind, projection)
        })
    }).collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(HarnessRuntimeError::DispatchPreparation(
            "control lifecycle event matches more than one run",
        ));
    }
    let Some((run_id, task_id, sequence, kind, projection)) = matches.into_iter().next() else {
        return Ok(EngineTouch::default());
    };
    commit_lifecycle_projection(
        harness,
        &run_id,
        &routed.node_id,
        routed.cursor.incarnation_id,
        sequence,
        kind,
        projection,
        now_unix_ms,
    )?;
    let mut touch = EngineTouch { task_ids: vec![task_id], run_ids: vec![run_id] };
    touch.merge(EngineTouch {
        task_ids: reconcile_task_result_refs(harness, now_unix_ms)?,
        run_ids: Vec::new(),
    });
    Ok(touch)
}

/// Returns every run+task id this call actually froze to `Waiting`, for the
/// same reason `apply_exact_control_lifecycle` returns its own touch set.
fn freeze_bound_route_waiting(
    harness: &mut HarnessService,
    route: &NodeRoute,
    event_sequence: u64,
    now_unix_ms: u64,
) -> Result<EngineTouch, HarnessRuntimeError> {
    let runs = harness.engine().runs().filter(|run| {
        run.lifecycle == HarnessRunLifecycleV1::Running
            && run.binding.as_ref().is_some_and(|binding| {
                binding.node_id.as_str() == route.node_id.as_str()
                    && binding.node_incarnation.as_str()
                        == route.expected_incarnation_id.to_string()
            })
    }).map(|run| (run.run_id.clone(), run.task_id.clone())).collect::<Vec<_>>();
    let mut touch = EngineTouch::default();
    for (run_id, task_id) in runs {
        commit_lifecycle_projection(
            harness,
            &run_id,
            &route.node_id,
            route.expected_incarnation_id,
            event_sequence,
            HarnessLifecycleEventKindV1::GapWaiting,
            HarnessLifecycleProjectionV1::Waiting,
            now_unix_ms,
        )?;
        touch.run_ids.push(run_id);
        touch.task_ids.push(task_id);
    }
    Ok(touch)
}

fn start_dispatch_preflight(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    intent: HarnessDispatchIntentV1,
) -> Result<(), HarnessRuntimeError> {
    let node_id = gate4agent_node_protocol::NodeId::new(intent.intent.node_id.as_str())
        .map_err(|_| HarnessRuntimeError::DispatchPreparation(
            "dispatch intent node id is not a valid node id",
        ))?;
    let route = adapter.exact_route(&node_id)?;
    let profile_id = SpawnProfileId::new(intent.intent.provider_profile.as_str())
        .map_err(|_| HarnessRuntimeError::DispatchPreparation(
            "dispatch intent provider profile is not a valid spawn profile id",
        ))?;
    let adapter = adapter.clone();
    let commands = commands.clone();
    tokio::spawn(async move {
        let result = adapter.preflight_spawn_profile(&route, &profile_id).await;
        let _ = commands.send(HostCommand::DispatchPreflightFinished { intent, result }).await;
    });
    Ok(())
}

fn start_native_history_worker(
    pending: PendingNativeHistoryRequest,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
) {
    tokio::spawn(async move {
        let reply_value = match pending.finish().await {
            Ok(response) => HarnessOperatorReplyV1::Ok { response },
            Err(error) => HarnessOperatorReplyV1::Error {
                error: map_native_history_error(error),
            },
        };
        let _ = reply.send(reply_value);
        let _ = commands.send(HostCommand::NativeHistoryWorkerFinished).await;
    });
}

fn start_run_read_worker(
    pending: PendingRunRead,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
) {
    tokio::spawn(async move {
        let completion = pending.finish().await;
        let _ = commands.send(HostCommand::RunReadFinished { completion, reply }).await;
    });
}

/// Node-scoped sibling of `start_run_read_worker`. No completion origin
/// re-check is needed on the way back: unlike a run's binding, a node route
/// cannot drift out from under an in-flight read — `PendingNodeWorkspaceRead::
/// finish` already rejects a response whose route or incarnation changed.
/// Spawns the node round trip and, unlike `start_run_read_worker`, races it
/// against `cancel` (populated only when the operator connection handler
/// created one — see `HostCommand::Operator::cancel`). When `cancel`
/// resolves first, this stops awaiting `pending.finish()` immediately
/// instead of running it to completion in the background: the harness-side
/// worker slot (`node_workspace_read_workers`, distinct from the node's own
/// `inspection_slots`) is released as soon as the cancel fires rather than
/// whenever the abandoned c2 round trip eventually settles on its own. The
/// node-side inspection budget (`GATE4AGENT_NODE_WORKSPACE_INSPECTION_
/// BUDGET_MS`) is what actually frees the *node's* permit; this only stops
/// the harness host from accumulating zombie awaits on its own side.
fn start_node_workspace_read_worker(
    pending: PendingNodeWorkspaceRead,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            "node workspace read worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::NodeWorkspaceReadCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        let _ = commands
            .send(HostCommand::NodeWorkspaceReadFinished { result, reply, identity })
            .await;
    });
}

/// Write/create sibling of `start_node_workspace_read_worker`: same bounded
/// worker shape, same cooperative-cancel race against the operator
/// connection's own deadline.
fn start_node_workspace_write_worker(
    pending: PendingNodeWorkspaceWrite,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            path = identity.path(),
                            "node workspace write worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::NodeWorkspaceWriteCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        let _ = commands
            .send(HostCommand::NodeWorkspaceWriteFinished { result, reply, identity })
            .await;
    });
}

/// Session-record-mutation-family sibling of `start_node_workspace_write_worker`:
/// same bounded-worker/cooperative-cancel shape, plus capturing `route`
/// before `pending.finish()` consumes it -- every settled mutation here
/// invalidates the route's runtime-inventory entry, unconditionally (see
/// `HostCommand::SessionRecordMutationFinished`'s handler).
fn start_session_record_mutation_worker(
    pending: PendingSessionRecordMutation,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        let route = pending.route().clone();
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            session_id = identity.session_id(),
                            "session record mutation worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::SessionRecordMutationCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        let _ = commands
            .send(HostCommand::SessionRecordMutationFinished { result, reply, identity, route })
            .await;
    });
}

/// Read-family sibling of `start_node_workspace_read_worker`: no roster
/// effect at all (a folder-browser page never mutates anything), so unlike
/// `start_resource_mutation_worker` there is no `route`/roster-effect
/// capture -- just the bounded-worker/cooperative-cancel shape every family
/// on this wire shares.
fn start_host_directory_browse_worker(
    pending: PendingHostDirectoryBrowse,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            path = identity.path(),
                            "host directory browse worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::HostDirectoryBrowseCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        let _ = commands
            .send(HostCommand::HostDirectoryBrowseFinished { result, reply, identity })
            .await;
    });
}

/// Resource-mutation-family sibling of `start_session_record_mutation_worker`:
/// same bounded-worker/cooperative-cancel shape, plus capturing `route` and
/// whether this kind invalidates the runtime-inventory roster (see
/// `ResourceMutationKind::invalidates_runtime_inventory`'s doc comment,
/// `c2.rs`) before `pending.finish()` consumes it.
fn start_resource_mutation_worker(
    pending: PendingResourceMutation,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        let route = pending.route().clone();
        let invalidates_runtime_inventory = pending.invalidates_runtime_inventory();
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            source_workspace_id = identity.source_workspace_id(),
                            path = identity.path(),
                            "resource mutation worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::ResourceMutationCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        let _ = commands
            .send(HostCommand::ResourceMutationFinished {
                result, reply, identity, route, invalidates_runtime_inventory,
            })
            .await;
    });
}

/// Direct operator `SpawnSession`: unlike `start_node_workspace_read_worker`
/// (which only awaits an already-enqueued round trip), the whole dispatch --
/// preflight, build, enqueue, await -- runs inside this task, because
/// preflight itself is an async C2 round trip (`preflight_spawn_profile`)
/// that cannot run inline in the synchronous host select loop. Races against
/// `cancel` the same way, for the same reason (see the doc comment on
/// `HostCommand::Operator::cancel`).
fn start_session_spawn_worker(
    adapter: HarnessC2Adapter,
    prepared: PreparedSessionSpawn,
    operation_id: HarnessOperationId,
    idempotency_ref: HarnessIdempotencyRef,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    let route = prepared.route().clone();
    let requested_transport = harness_transport_for_mode(prepared.mode());
    tokio::spawn(async move {
        let dispatch = adapter.dispatch_session_spawn(prepared, operation_id, idempotency_ref);
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = dispatch => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            provider = identity.provider(),
                            provider_profile = identity.provider_profile(),
                            "session spawn worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::SessionSpawnCancelled)
                    }
                }
            }
            None => dispatch.await,
        };
        let _ = commands
            .send(HostCommand::SessionSpawnFinished {
                route,
                result,
                reply,
                identity,
                requested_transport,
            })
            .await;
    });
}

/// Node-scoped sibling of `start_node_workspace_read_worker`: the request is
/// already enqueued (`adapter.start_prepared_session_control`), this only
/// awaits the reply and races the same cooperative cancel.
fn start_session_control_worker(
    adapter: HarnessC2Adapter,
    pending: PendingSessionControl,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
    identity: OperatorRequestLogIdentity,
    cancel: Option<oneshot::Receiver<()>>,
) {
    tokio::spawn(async move {
        // Captured before `pending.finish()` consumes it below.
        let route = pending.route().clone();
        let stopped_session = pending.stop_session_address();
        let roster_effect = pending.roster_effect();
        let result = match cancel {
            Some(cancel) => {
                tokio::select! {
                    result = pending.finish() => result,
                    _ = cancel => {
                        tracing::warn!(
                            operation = %identity.operation,
                            node_id = identity.node_id(),
                            workspace_id = identity.workspace_id(),
                            session_id = identity.session_id(),
                            "session control worker cancelled: the operator connection's own deadline fired before the node replied",
                        );
                        Err(HarnessC2Error::SessionControlCancelled)
                    }
                }
            }
            None => pending.finish().await,
        };
        // A settled `StopSession` needs a follow-up node-side reap (see
        // `HarnessC2Adapter::remove_stopped_session`'s doc comment). Taken
        // before `stopped_session` moves below, but only acted on after the
        // `HostCommand` is sent -- the operator's own `StopSession` reply
        // travels through the host loop from that send, so a slow or failed
        // reap must never delay or fail the verb itself.
        let reap = result.is_ok().then(|| stopped_session.clone()).flatten();
        let roster_effect = if result.is_ok() { roster_effect } else { SessionRosterEffect::None };
        let _ = commands
            .send(HostCommand::SessionControlFinished {
                result,
                reply,
                identity,
                route: route.clone(),
                roster_effect,
            })
            .await;
        if let Some(session) = reap {
            if let Err(error) = adapter.remove_stopped_session(&route, session).await {
                tracing::warn!(
                    node_id = route.node_id.as_str(),
                    cause = %error,
                    "force-stopped session failed to reap from the node's runtime inventory; it remains reported until removed",
                );
            }
        }
    });
}

/// Background twin of `start_run_read_worker`: no live client is waiting,
/// so there is no `reply` sender — the completion travels back into the
/// host loop as a plain `HostCommand` for `finish_run_git_facts_capture` to
/// resolve.
fn start_run_git_facts_capture_worker(
    pending: PendingRunRead,
    run_id: gate4agent_harness_protocol::HarnessRunId,
    commands: mpsc::Sender<HostCommand>,
) {
    tokio::spawn(async move {
        let completion = pending.finish().await;
        let _ = commands.send(
            HostCommand::RunGitFactsCaptureFinished { run_id, completion },
        ).await;
    });
}

fn start_run_context_source_worker(
    pending: PendingRunContextSourceObservation,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
) {
    tokio::spawn(async move {
        let completion = pending.finish().await;
        let _ = commands.send(HostCommand::RunContextSourceFinished {
            completion,
            reply,
        }).await;
    });
}

fn map_run_context_source_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::RunContextSourceEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::RunContextSourceEnqueue(_)
        | HarnessC2Error::RunContextSourceTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::RunContextSourceRouteMismatch => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::RunContextSourceDeadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::RunContextSourceRejected { code } => match code {
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            NodeFailureCode::UnknownSessionRecord => HarnessOperatorHostErrorV1::NotFound,
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::SessionRecordConflict
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::RunContextSourceUnbound
        | HarnessC2Error::RunContextSourceUnsupportedBinding => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::InvalidRunContextSourceBinding
        | HarnessC2Error::RunContextSourceCorrelationMismatch
        | HarnessC2Error::RunContextSourceProjection => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn map_run_read_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidRunReadRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessC2Error::RunReadUnbound => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::RunReadEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::RunReadEnqueue(_)
        | HarnessC2Error::RunReadTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::RunReadRouteMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::RunReadDeadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::RunReadTooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessC2Error::RunReadRejected { code } => match code {
            NodeFailureCode::InvalidRequest
            | NodeFailureCode::InvalidRepositoryPath => {
                HarnessOperatorHostErrorV1::InvalidRequest
            }
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::RepositoryFileNotFound
            | NodeFailureCode::RepositoryParentNotFound => {
                HarnessOperatorHostErrorV1::NotFound
            }
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::RepositoryFileRevisionConflict
            | NodeFailureCode::RepositoryFileNotRegular
            | NodeFailureCode::RepositoryPathUnsafe
            | NodeFailureCode::NotGitRepository
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::RepositoryFileReadTimedOut
            | NodeFailureCode::GitReadTimedOut
            | NodeFailureCode::HostDirectoryReadTimedOut
            | NodeFailureCode::SpawnDeadlineExceeded => {
                HarnessOperatorHostErrorV1::Deadline
            }
            NodeFailureCode::ResponseTooLarge => HarnessOperatorHostErrorV1::TooLarge,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::RepositoryFileReadFailed
            | NodeFailureCode::GitReadFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::InvalidRunReadBinding
        | HarnessC2Error::RunReadCorrelationMismatch
        | HarnessC2Error::RunReadProjection => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// Node-scoped sibling of `map_run_read_error`. The `NodeFailureCode` branch
/// is intentionally kept in lockstep with `map_run_read_error`'s: both read
/// families relay the exact same `NodeRequest::InspectWorkspace`/
/// `ReadWorkspaceFile`/`ReadGitHistory`/`ReadGitDiff` verbs, so the Node
/// fails them the same way regardless of which harness read family asked.
fn map_node_workspace_read_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidNodeWorkspaceReadRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::NodeWorkspaceReadEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::NodeWorkspaceReadEnqueue(_)
        | HarnessC2Error::NodeWorkspaceReadTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::NodeWorkspaceReadRouteMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::NodeWorkspaceReadDeadline
        | HarnessC2Error::NodeWorkspaceReadCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::NodeWorkspaceReadTooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessC2Error::NodeWorkspaceReadRejected { code } => match code {
            NodeFailureCode::InvalidRequest
            | NodeFailureCode::InvalidRepositoryPath => {
                HarnessOperatorHostErrorV1::InvalidRequest
            }
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::RepositoryFileNotFound
            | NodeFailureCode::RepositoryParentNotFound => {
                HarnessOperatorHostErrorV1::NotFound
            }
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::RepositoryFileRevisionConflict
            | NodeFailureCode::RepositoryFileNotRegular
            | NodeFailureCode::RepositoryPathUnsafe
            | NodeFailureCode::NotGitRepository
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::RepositoryFileReadTimedOut
            | NodeFailureCode::GitReadTimedOut
            | NodeFailureCode::HostDirectoryReadTimedOut
            | NodeFailureCode::SpawnDeadlineExceeded => {
                HarnessOperatorHostErrorV1::Deadline
            }
            NodeFailureCode::ResponseTooLarge => HarnessOperatorHostErrorV1::TooLarge,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::RepositoryFileReadFailed
            | NodeFailureCode::GitReadFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::NodeWorkspaceReadCorrelationMismatch
        | HarnessC2Error::NodeWorkspaceReadProjection => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// Write/create sibling of `map_node_workspace_read_error`. Unlike that
/// function (whose `NodeFailureCode` branch is kept in lockstep with
/// `map_run_read_error`'s, since both read families relay identical
/// `NodeRequest` verbs), a write's failure surface is a genuine superset of
/// a read's: `WriteWorkspaceFile`'s CAS can be rejected with
/// `RepositoryFileRevisionConflict` (the stale-`expected_revision` case --
/// surfaces as `Conflict`, never collapsed into `Internal`), and
/// `CreateWorkspaceFile`/`CreateWorkspaceDirectory` can be rejected with the
/// entry-creation-specific codes below that a read never produces.
fn map_node_workspace_write_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidNodeWorkspaceWriteRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::NodeWorkspaceWriteEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::NodeWorkspaceWriteEnqueue(_)
        | HarnessC2Error::NodeWorkspaceWriteTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::NodeWorkspaceWriteRouteMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::NodeWorkspaceWriteDeadline
        | HarnessC2Error::NodeWorkspaceWriteCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::NodeWorkspaceWriteTooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessC2Error::NodeWorkspaceWriteRejected { code } => match code {
            NodeFailureCode::InvalidRequest
            | NodeFailureCode::InvalidRepositoryPath => HarnessOperatorHostErrorV1::InvalidRequest,
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::RepositoryFileNotFound
            | NodeFailureCode::RepositoryParentNotFound => HarnessOperatorHostErrorV1::NotFound,
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::RepositoryFileRevisionConflict
            | NodeFailureCode::RepositoryFileNotRegular
            | NodeFailureCode::RepositoryPathUnsafe
            | NodeFailureCode::RepositoryEntryAlreadyExists
            | NodeFailureCode::RepositoryParentNotDirectory
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::RepositoryFileWriteTimedOut
            | NodeFailureCode::RepositoryEntryCreateTimedOut
            | NodeFailureCode::SpawnDeadlineExceeded => HarnessOperatorHostErrorV1::Deadline,
            NodeFailureCode::ResponseTooLarge => HarnessOperatorHostErrorV1::TooLarge,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::RepositoryFileWriteFailed
            | NodeFailureCode::RepositoryEntryCreateFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::NodeWorkspaceWriteCorrelationMismatch
        | HarnessC2Error::NodeWorkspaceWriteProjection => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// Maps everything `HarnessC2Adapter::dispatch_session_spawn` can return
/// before or around the C2 round trip. A `SpawnDispatchOutcome::Rejected`/
/// `OutcomeUnknown` reply (the round trip actually happened) is handled
/// separately by `map_session_spawn_node_failure`/the `OutcomeUnknown`
/// host error, not here -- this only covers the `Err(HarnessC2Error)` path.
fn map_session_spawn_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidSessionSpawnRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. } => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::SpawnProfileUnavailable(_) => HarnessOperatorHostErrorV1::NotFound,
        HarnessC2Error::SpawnEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::SpawnEnqueue(_) => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::SessionSpawnCancelled => HarnessOperatorHostErrorV1::Deadline,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// The transport a `HarnessExecutionModeV1` requests -- exact mirror of
/// `gate4agent-node`'s own `SessionMode` -> `TransportKind` match (`server.rs`)
/// one layer further out, kept in the wire's own `HarnessRuntimeTransportV1`
/// vocabulary rather than the node's. Only needed to name the transport a
/// rejected spawn asked for (`map_session_spawn_node_failure`'s
/// `agent`/`requested_transport` parameters); every other spawn path already
/// has no need to know this.
fn harness_transport_for_mode(mode: HarnessExecutionModeV1) -> HarnessRuntimeTransportV1 {
    match mode {
        HarnessExecutionModeV1::Pty => HarnessRuntimeTransportV1::Pty,
        HarnessExecutionModeV1::Inline => HarnessRuntimeTransportV1::Pipe,
        HarnessExecutionModeV1::Acp => HarnessRuntimeTransportV1::Acp,
    }
}

/// `NodeFailureCode` branch for a spawn actually rejected by the Node (a
/// `SpawnDispatchOutcome::Rejected{code}` reply, not a transport failure).
/// `agent`/`requested_transport` name the exact provider/transport the
/// caller's own `SpawnSession` request asked for -- the node's response
/// carries only the bare `code`, so `UnsupportedTransport`'s typed operator-
/// wire payload is built from what this request's own caller
/// (`start_session_spawn_worker`) already had in hand, not from anything
/// decoded out of the node's reply.
fn map_session_spawn_node_failure(
    code: NodeFailureCode,
    agent: &str,
    requested_transport: HarnessRuntimeTransportV1,
) -> HarnessOperatorHostErrorV1 {
    match code {
        NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        NodeFailureCode::UnknownWorkspace => HarnessOperatorHostErrorV1::NotFound,
        NodeFailureCode::SpawnProfileRevisionMismatch
        | NodeFailureCode::BindingMismatch
        | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
        NodeFailureCode::ControllerBusy
        | NodeFailureCode::WorkspaceBusy
        | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
        NodeFailureCode::SpawnDeadlineExceeded => HarnessOperatorHostErrorV1::Deadline,
        // Named separately from the generic backend-failure bucket right
        // below so the operator can tell "this exact provider/transport
        // combination is not supported" apart from "the node is busy or
        // unavailable for reasons unrelated to what was asked" -- see
        // `HarnessOperatorHostErrorV1::UnsupportedTransport`'s own doc.
        NodeFailureCode::UnsupportedTransport => HarnessOperatorHostErrorV1::UnsupportedTransport {
            agent: agent.to_owned(),
            transport: requested_transport,
        },
        // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
        // UnsupportedCapability`'s own doc for why this no longer folds into
        // `Unavailable`.
        NodeFailureCode::UnsupportedCapability => {
            HarnessOperatorHostErrorV1::UnsupportedCapability
        }
        NodeFailureCode::BackendDisconnected
        | NodeFailureCode::BackendOperationFailed
        | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// `WriteSessionInput`/`ResizeSession`/`StopSession`/`ControlSession`/
/// `WriteSessionBytes`/`PasteSession`/`RemoveSession`/`ResumeSession`, plus
/// the four ACP control verbs `ResolveInteraction`/`SetSessionMode`/
/// `SetSessionConfigOption`/`SetSessionModel`, plus `PromptSession`, share
/// one C2 relay shape (`PreparedSessionControl`/`PendingSessionControl::
/// finish`), so one mapper covers all thirteen -- unlike spawn, none of
/// these carries a multi-outcome transport ambiguity worth its own host
/// error (DECISIONS: "naturally idempotent-enough", no dedup, no
/// `OutcomeUnknown` case here). Every `NodeFailureCode` the node can answer
/// any of the thirteen with is bucketed below by its own meaning
/// (`NotFound`/`Conflict`/`Busy`/`Unavailable`/`InvalidRequest`), never
/// collapsed wholesale into `Internal` -- `PromptSession`'s OWN named PTY
/// refusal happens earlier, in `prompt_session_pty_refusal`, before a
/// confirmed-PTY target ever reaches C2/this mapper. `PromptSession`/
/// `PasteSession` against an ACP or inline session DOES add one code of its
/// own that reaches here, though: `NodeFailureCode::TurnInFlight` (the
/// node's own turn-admission gate -- see `require_session_runtime_policy`
/// in `gate4agent-node`'s `server.rs`), bucketed as `Conflict` below like
/// every other "current session state disallows this request" code already
/// is, not folded into `Internal`.
fn map_session_control_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidSessionControlRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessC2Error::SessionControlEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::SessionControlEnqueue(_)
        | HarnessC2Error::SessionControlTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::SessionControlRouteMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::SessionControlCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::SessionControlRejected { code } => match code {
            NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            // The unknown-target bucket every other mapper on this wire
            // already has (`map_resource_mutation_error` buckets these two
            // exactly this way). Without it both fell through to
            // `Internal`, so all eight session-control verbs answered
            // "the host broke" for a session that is simply not there any
            // more -- observed live: a `RemoveSession` for a session the
            // node had already reaped during its own `Stop` came back
            // `internal`, and reading it as a failure of the remove cost
            // an investigation that ended at this arm.
            NodeFailureCode::UnknownSession | NodeFailureCode::UnknownWorkspace => {
                HarnessOperatorHostErrorV1::NotFound
            }
            // `TurnInFlight` -- `Prompt`/`Paste` refused against a session
            // that already has a provider turn running -- is a state
            // conflict, the same bucket every other "current session state
            // disallows this request" code above already uses.
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::StaleGeneration
            | NodeFailureCode::TurnInFlight => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn map_session_record_mutation_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidSessionRecordMutationRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::SessionRecordMutationEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::SessionRecordMutationEnqueue(_)
        | HarnessC2Error::SessionRecordMutationTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::SessionRecordMutationRouteMismatch
        | HarnessC2Error::SessionRecordMutationCorrelationMismatch => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::SessionRecordMutationDeadline
        | HarnessC2Error::SessionRecordMutationCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::SessionRecordMutationProjection => HarnessOperatorHostErrorV1::Internal,
        HarnessC2Error::SessionRecordMutationRejected { code } => match code {
            NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            NodeFailureCode::UnknownSessionRecord | NodeFailureCode::UnknownWorkspace => {
                HarnessOperatorHostErrorV1::NotFound
            }
            NodeFailureCode::SessionRecordConflict
            | NodeFailureCode::SessionRecordNotResumable
            | NodeFailureCode::SessionWorkspaceMismatch
            | NodeFailureCode::WorkspaceRegistrationRequired
            | NodeFailureCode::BindingMismatch
            | NodeFailureCode::StaleGeneration
            | NodeFailureCode::StaleNativeSessionCatalog => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::SessionRecordBusy
            | NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn map_host_directory_browse_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidHostDirectoryBrowseRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::HostDirectoryBrowseEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::HostDirectoryBrowseEnqueue(_)
        | HarnessC2Error::HostDirectoryBrowseTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::HostDirectoryBrowseRouteMismatch
        | HarnessC2Error::HostDirectoryBrowseCorrelationMismatch => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::HostDirectoryBrowseDeadline
        | HarnessC2Error::HostDirectoryBrowseCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::HostDirectoryBrowseProjection => HarnessOperatorHostErrorV1::Internal,
        HarnessC2Error::HostDirectoryBrowseRejected { code } => match code {
            NodeFailureCode::InvalidRequest | NodeFailureCode::HostDirectoryInvalid => {
                HarnessOperatorHostErrorV1::InvalidRequest
            }
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::HostDirectoryReadTimedOut => HarnessOperatorHostErrorV1::Deadline,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::HostDirectoryReadFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

/// Covers every `NodeFailureCode` plausible for the seven heterogeneous
/// verbs `ResourceMutationKind` relays (workspace/worktree lifecycle,
/// context-pack export/forget) with the same semantic bucketing every other
/// map function on this wire already uses (invalid-shape -> `InvalidRequest`,
/// unknown-target -> `NotFound`, already-exists/in-a-state-that-conflicts ->
/// `Conflict`, contended -> `Busy`, timed-out -> `Deadline`, unsupported/
/// disconnected -> `Unavailable`), not a per-verb table: unlike the session-
/// record-mutation family (six verbs sharing a narrower, more homogeneous
/// failure surface), a git-worktree operation's failure surface (protected/
/// dirty/locked worktrees, duplicate workspace ids/roots, recovery-required
/// states) does not cleanly overlap the context-pack family's, so this
/// mapper covers both by node-failure-code meaning rather than by verb.
fn map_resource_mutation_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidResourceMutationRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessC2Error::ResourceMutationEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::ResourceMutationEnqueue(_)
        | HarnessC2Error::ResourceMutationTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::ResourceMutationRouteMismatch
        | HarnessC2Error::ResourceMutationCorrelationMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::ResourceMutationDeadline
        | HarnessC2Error::ResourceMutationCancelled => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::ResourceMutationProjection => HarnessOperatorHostErrorV1::Internal,
        HarnessC2Error::ResourceMutationRejected { code } => match code {
            NodeFailureCode::InvalidRequest
            | NodeFailureCode::InvalidWorkspaceRoot
            | NodeFailureCode::InvalidRepositoryPath => HarnessOperatorHostErrorV1::InvalidRequest,
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::UnknownSession
            | NodeFailureCode::UnknownContextPack
            | NodeFailureCode::NotGitRepository => HarnessOperatorHostErrorV1::NotFound,
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::DuplicateWorkspaceId
            | NodeFailureCode::DuplicateWorkspaceRoot
            | NodeFailureCode::LastWorkspace
            | NodeFailureCode::WorktreeConflict
            | NodeFailureCode::WorktreeProtected
            | NodeFailureCode::WorktreeDirty
            | NodeFailureCode::WorktreeLocked
            | NodeFailureCode::WorkspaceRegistrationRequired
            | NodeFailureCode::StandaloneWorkspaceRecoveryRequired
            | NodeFailureCode::ManagedWorktreeRecoveryRequired
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::ControllerRequired
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy
            | NodeFailureCode::ContextPackBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::SpawnDeadlineExceeded => HarnessOperatorHostErrorV1::Deadline,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::ContextPackMaterializationFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn map_native_history_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidNativeHistoryRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::NativeHistoryEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::NativeHistoryEnqueue(_)
        | HarnessC2Error::NativeHistoryTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_)
        | HarnessC2Error::RelayReconnecting => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::NativeHistoryRouteMismatch => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::NativeHistoryDeadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::NativeHistoryRejected { code } => match code {
            NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            // `PreviewSessionRecord` rides this same pool (see
            // `is_native_history_request`'s doc comment) and is the only
            // verb here that can fail with this code (an unknown
            // `record_id`); the other three verbs in the pool never
            // produce it. `UnknownWorkspace` stays mapped to `Unavailable`
            // below, unchanged, rather than moving here -- this new arm is
            // additive only.
            NodeFailureCode::UnknownSessionRecord => HarnessOperatorHostErrorV1::NotFound,
            NodeFailureCode::StaleNativeSessionCatalog => {
                HarnessOperatorHostErrorV1::Conflict
            }
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            // Permanent, not transient -- see `HarnessOperatorHostErrorV1::
            // UnsupportedCapability`'s own doc for why this no longer folds
            // into `Unavailable`.
            NodeFailureCode::UnsupportedCapability => {
                HarnessOperatorHostErrorV1::UnsupportedCapability
            }
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::NativeHistoryCorrelationMismatch => {
            HarnessOperatorHostErrorV1::Internal
        }
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn is_native_history_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::CatalogNativeSessions { .. }
            | HarnessOperatorRequestV1::PageNativeSessions { .. }
            | HarnessOperatorRequestV1::PreviewNativeSession { .. }
            // `PreviewSessionRecord` relays the exact same `NodeRequest::
            // PreviewSessionRecord` the light TUI's initial-preview-open and
            // background-history-refresh actions both send (see the doc
            // comment on `HarnessOperatorRequestV1::PreviewSessionRecord`),
            // so it rides this same read-worker pool rather than getting its
            // own.
            | HarnessOperatorRequestV1::PreviewSessionRecord { .. }
    )
}

/// The session-record mutation family: `ResumeSessionRecord`/
/// `RenameSessionRecord`/`SetSessionTask`/`ForgetSessionRecord`/
/// `IndexProviderSession`/`IndexNativeSession` -- see
/// `SessionRecordMutationKind`'s doc comment (`c2.rs`) for why this rides its
/// own bounded pool rather than either `is_session_control_request`'s pool
/// (thin ack-only session-address-scoped verbs) or `is_native_history_
/// request`'s pool (reads, no roster invalidation on success).
fn is_session_record_mutation_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::ResumeSessionRecord { .. }
            | HarnessOperatorRequestV1::RenameSessionRecord { .. }
            | HarnessOperatorRequestV1::SetSessionTask { .. }
            | HarnessOperatorRequestV1::ForgetSessionRecord { .. }
            | HarnessOperatorRequestV1::IndexProviderSession { .. }
            | HarnessOperatorRequestV1::IndexNativeSession { .. }
    )
}

/// `BrowseHostDirectories`: the folder-browser dialog's paged host-directory
/// listing. Node-scoped only (no `workspace_id`) -- see
/// `PreparedHostDirectoryBrowse`'s doc comment (`c2.rs`) for why this does
/// not ride `is_node_workspace_read_request`'s pool.
fn is_host_directory_browse_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(request, HarnessOperatorRequestV1::BrowseHostDirectories { .. })
}

/// The resource-mutation family: `RegisterWorkspace`/`UnregisterWorkspace`/
/// `CreateStandaloneWorkspace`/`CreateWorktree`/`RemoveWorktree`/
/// `ExportContextPack`/`ForgetContextPack` -- see `ResourceMutationKind`'s
/// doc comment (`c2.rs`) for why these seven heterogeneous verbs share one
/// bounded pool.
fn is_resource_mutation_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::RegisterWorkspace { .. }
            | HarnessOperatorRequestV1::UnregisterWorkspace { .. }
            | HarnessOperatorRequestV1::CreateStandaloneWorkspace { .. }
            | HarnessOperatorRequestV1::CreateWorktree { .. }
            | HarnessOperatorRequestV1::RemoveWorktree { .. }
            | HarnessOperatorRequestV1::ExportContextPack { .. }
            | HarnessOperatorRequestV1::ForgetContextPack { .. }
    )
}

fn is_run_read_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::InspectRunWorkspace { .. }
            | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
            | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
            | HarnessOperatorRequestV1::ReadRunGitDiff { .. }
    )
}

/// Node-scoped sibling of `is_run_read_request`: sidebar Files/Git reads
/// that have no run in flight, routed directly from a node/workspace pair.
fn is_node_workspace_read_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::InspectNodeWorkspace { .. }
            | HarnessOperatorRequestV1::ReadNodeWorkspaceFile { .. }
            | HarnessOperatorRequestV1::ReadNodeGitHistory { .. }
            | HarnessOperatorRequestV1::ReadNodeGitDiff { .. }
    )
}

/// Write/create sibling of `is_node_workspace_read_request`: the editor
/// save and file/directory creation verbs, routed the same way (live
/// `exact_route`, no run in flight, own bounded worker pool).
fn is_node_workspace_write_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::WriteNodeWorkspaceFile { .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceFile { .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory { .. }
    )
}

fn is_run_context_source_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(request, HarnessOperatorRequestV1::ObserveRunContextSource { .. })
}

/// Direct operator spawn: no Task/Run/plan, see
/// `HarnessOperatorRequestV1::SpawnSession`'s doc comment.
fn is_session_spawn_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(request, HarnessOperatorRequestV1::SpawnSession { .. })
}

/// The twelve thin session-control verbs sharing one C2 relay shape --
/// `PreparedSessionControl`/`PendingSessionControl` in `c2.rs` -- the eight
/// original terminal/session-lifecycle verbs plus the four ACP control
/// verbs (`ResolveInteraction`/`SetSessionMode`/`SetSessionConfigOption`/
/// `SetSessionModel`), which joined the same family (see
/// `SessionControlKind`'s own doc comment in `c2.rs`).
fn is_session_control_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::WriteSessionInput { .. }
            | HarnessOperatorRequestV1::PromptSession { .. }
            | HarnessOperatorRequestV1::ResizeSession { .. }
            | HarnessOperatorRequestV1::StopSession { .. }
            | HarnessOperatorRequestV1::ControlSession { .. }
            | HarnessOperatorRequestV1::WriteSessionBytes { .. }
            | HarnessOperatorRequestV1::PasteSession { .. }
            | HarnessOperatorRequestV1::RemoveSession { .. }
            | HarnessOperatorRequestV1::ResumeSession { .. }
            | HarnessOperatorRequestV1::ResolveInteraction { .. }
            | HarnessOperatorRequestV1::SetSessionMode { .. }
            | HarnessOperatorRequestV1::SetSessionConfigOption { .. }
            | HarnessOperatorRequestV1::SetSessionModel { .. }
    )
}

/// Best-effort, log-only identity of an operator request: which operation it
/// is plus whichever of node id / workspace id / run id applies. Captured
/// from `&HarnessOperatorRequestV1` before the request is moved into the
/// host command queue, so every rejection site downstream (queued-request
/// validation failure, harness-side worker-capacity busy, C2 dispatch
/// failure, the deadline branch in `handle_connection`) can log a
/// self-contained line without re-deriving identity from whatever is left
/// of the request at that point (which, past several of these sites, is
/// nothing — the request has already been consumed). `operation` is read
/// from the request's own serde `kind` tag rather than hand-matched, so it
/// can never drift from the wire discriminant as request variants are
/// added.
///
/// Promoted `pub` for `gate4agent-harness-light`: its own `SubscribeEvents`
/// branch (A3) needs an identity to hand `SubscriberRegistry::insert`, and
/// this is exactly the same pure `&HarnessOperatorRequestV1 ->` identity
/// mapping either harness wants, with zero kernel entanglement.
#[derive(Clone, Debug)]
pub struct OperatorRequestLogIdentity {
    // `pub(crate)`, not private: `terminal::TerminalSubscriberRegistry::
    // remove_at` (a different module in this crate) logs a closed
    // subscriber the same way `SubscriberRegistry::remove_at` below does,
    // and needs this field directly for the same `tracing::info!` shape.
    pub(crate) operation: String,
    node_id: Option<String>,
    workspace_id: Option<String>,
    run_id: Option<String>,
    session_id: Option<String>,
    provider: Option<String>,
    provider_profile: Option<String>,
    path: Option<String>,
    /// Only the worktree family (`CreateWorktree`/`RemoveWorktree`) carries a
    /// second workspace id worth logging: `workspace_id` above holds the
    /// worktree's own (new, or -- for a remove -- absent) id, this field
    /// holds the workspace the worktree is created from/removed against.
    source_workspace_id: Option<String>,
}

impl OperatorRequestLogIdentity {
    pub fn describe(request: &HarnessOperatorRequestV1) -> Self {
        let (node_id, workspace_id, session_id) = match request {
            HarnessOperatorRequestV1::InspectNodeWorkspace { node_id, workspace_id }
            | HarnessOperatorRequestV1::ReadNodeWorkspaceFile { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::ReadNodeGitHistory { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::ReadNodeGitDiff { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::WriteNodeWorkspaceFile { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceFile { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::SpawnSession { node_id, workspace_id, .. } => {
                (Some(node_id.clone()), Some(workspace_id.clone()), None)
            }
            HarnessOperatorRequestV1::WriteSessionInput { session, .. }
            | HarnessOperatorRequestV1::PromptSession { session, .. }
            | HarnessOperatorRequestV1::ResizeSession { session, .. }
            | HarnessOperatorRequestV1::StopSession { session, .. }
            | HarnessOperatorRequestV1::ControlSession { session, .. }
            | HarnessOperatorRequestV1::WriteSessionBytes { session, .. }
            | HarnessOperatorRequestV1::PasteSession { session, .. }
            | HarnessOperatorRequestV1::RemoveSession { session, .. }
            | HarnessOperatorRequestV1::ResumeSession { session, .. }
            | HarnessOperatorRequestV1::ResolveInteraction { session, .. }
            | HarnessOperatorRequestV1::SetSessionMode { session, .. }
            | HarnessOperatorRequestV1::SetSessionConfigOption { session, .. }
            | HarnessOperatorRequestV1::SetSessionModel { session, .. } => (
                Some(session.node_id.clone()),
                Some(session.workspace_id.clone()),
                Some(format!("{}/{}", session.instance_id, session.generation)),
            ),
            HarnessOperatorRequestV1::ResumeSessionRecord { node_id, record_id, .. }
            | HarnessOperatorRequestV1::RenameSessionRecord { node_id, record_id, .. }
            | HarnessOperatorRequestV1::SetSessionTask { node_id, record_id, .. }
            | HarnessOperatorRequestV1::ForgetSessionRecord { node_id, record_id } => {
                (Some(node_id.clone()), None, Some(record_id.clone()))
            }
            HarnessOperatorRequestV1::IndexProviderSession { node_id, workspace_id, .. } => {
                (Some(node_id.clone()), Some(workspace_id.clone()), None)
            }
            // This request has no session id by design -- creating the first
            // durable identity for a session that has none yet is the whole
            // point of it -- so the slot used to be left empty and every
            // warning for this operation printed `session_id=""`, which reads
            // as a lost id rather than as an absent one. The candidate being
            // promoted is right here, so name it: the same slot the arm above
            // fills with a record id.
            HarnessOperatorRequestV1::IndexNativeSession { selection, .. } => (
                Some(selection.route.node_id.clone()),
                selection.route.workspace_id.clone(),
                Some(selection.selection_id.clone()),
            ),
            HarnessOperatorRequestV1::BrowseHostDirectories { node_id, .. }
            | HarnessOperatorRequestV1::RemoveWorktree { node_id, .. } => {
                (Some(node_id.clone()), None, None)
            }
            HarnessOperatorRequestV1::RegisterWorkspace { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::UnregisterWorkspace { node_id, workspace_id }
            | HarnessOperatorRequestV1::CreateStandaloneWorkspace { node_id, workspace_id, .. }
            | HarnessOperatorRequestV1::CreateWorktree { node_id, workspace_id, .. } => {
                (Some(node_id.clone()), Some(workspace_id.clone()), None)
            }
            HarnessOperatorRequestV1::ExportContextPack { session } => (
                Some(session.node_id.clone()),
                Some(session.workspace_id.clone()),
                Some(format!("{}/{}", session.instance_id, session.generation)),
            ),
            HarnessOperatorRequestV1::ForgetContextPack { node_id, context_id } => {
                (Some(node_id.clone()), None, Some(context_id.as_str().to_owned()))
            }
            _ => (None, None, None),
        };
        let source_workspace_id = match request {
            HarnessOperatorRequestV1::CreateWorktree { source_workspace_id, .. }
            | HarnessOperatorRequestV1::RemoveWorktree { source_workspace_id, .. } => {
                Some(source_workspace_id.clone())
            }
            _ => None,
        };
        let run_id = match request {
            HarnessOperatorRequestV1::InspectRunWorkspace { run_id }
            | HarnessOperatorRequestV1::ReadRunWorkspaceFile { run_id, .. }
            | HarnessOperatorRequestV1::ReadRunGitHistory { run_id, .. }
            | HarnessOperatorRequestV1::ReadRunGitDiff { run_id, .. } => {
                Some(run_id.as_str().to_owned())
            }
            _ => None,
        };
        let (provider, provider_profile) = match request {
            HarnessOperatorRequestV1::SpawnSession { provider, provider_profile, .. } => {
                (Some(provider.clone()), Some(provider_profile.clone()))
            }
            _ => (None, None),
        };
        // Only the node-workspace write family carries a path worth logging
        // here: the read family's rejection/success sites never referenced
        // one before this, and adding it there is out of scope for this
        // change -- see `WriteNodeWorkspaceFile`/`CreateNodeWorkspaceFile`/
        // `CreateNodeWorkspaceDirectory`'s call sites in the host select
        // loop and `HostCommand::NodeWorkspaceWriteFinished`'s handler.
        // `root`/`target_root`/`directory` share this same field with the
        // node-workspace-write family's repository-relative `path` above:
        // both are "the path this request concerns" for logging purposes,
        // even though a host path and a repository-relative path are
        // different wire types.
        let path = match request {
            HarnessOperatorRequestV1::WriteNodeWorkspaceFile { path, .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceFile { path, .. }
            | HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory { path, .. } => {
                Some(path.as_str().to_owned())
            }
            HarnessOperatorRequestV1::BrowseHostDirectories { directory, .. } => {
                directory.as_ref().map(|value| value.as_str().to_owned())
            }
            HarnessOperatorRequestV1::RegisterWorkspace { root, .. }
            | HarnessOperatorRequestV1::CreateStandaloneWorkspace { root, .. } => {
                Some(root.as_str().to_owned())
            }
            HarnessOperatorRequestV1::CreateWorktree { target_root, .. }
            | HarnessOperatorRequestV1::RemoveWorktree { target_root, .. } => {
                Some(target_root.as_str().to_owned())
            }
            _ => None,
        };
        let operation = serde_json::to_value(request)
            .ok()
            .and_then(|value| {
                value.get("kind").and_then(|kind| kind.as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Self {
            operation, node_id, workspace_id, run_id, session_id, provider, provider_profile, path,
            source_workspace_id,
        }
    }

    // `pub(crate)`, same reason as the `operation` field above: `terminal::
    // TerminalSubscriberRegistry::remove_at` reads both accessors.
    pub(crate) fn node_id(&self) -> &str {
        self.node_id.as_deref().unwrap_or("")
    }

    pub(crate) fn workspace_id(&self) -> &str {
        self.workspace_id.as_deref().unwrap_or("")
    }

    fn run_id(&self) -> &str {
        self.run_id.as_deref().unwrap_or("")
    }

    fn session_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or("")
    }

    fn provider(&self) -> &str {
        self.provider.as_deref().unwrap_or("")
    }

    fn provider_profile(&self) -> &str {
        self.provider_profile.as_deref().unwrap_or("")
    }

    fn path(&self) -> &str {
        self.path.as_deref().unwrap_or("")
    }

    fn source_workspace_id(&self) -> &str {
        self.source_workspace_id.as_deref().unwrap_or("")
    }
}

/// Establishing a subscription is not a cheap verb. Its reply carries a
/// baseline -- every task, every run, and the whole runtime inventory --
/// which walks the same node reads the inventory verbs do, and those are
/// bounded in seconds each on their own.
///
/// It used to fall through to the generic three-second default, and so it
/// could not finish: the harness answered `deadline` and closed, every
/// single time, measured at three seconds flat against a live stack. The
/// consequence was not "the subscription drops sometimes" -- it was that a
/// subscription NEVER established, so the app never learned about anything
/// it had not polled for, and a session spawned right beside it stayed
/// invisible. The two-minute rhythm this looked like from the outside was
/// the client's own retry, not a subscription's lifetime.
///
/// Sized like the other heavy classes above and kept under
/// `HOST_CONNECTION_DEADLINE`, so the connection bound still has the last
/// word.
const HOST_SUBSCRIBE_RESPONSE_DEADLINE: Duration = Duration::from_secs(40);

fn operator_response_deadline(request: &HarnessOperatorRequestV1) -> Duration {
    if matches!(request, HarnessOperatorRequestV1::SubscribeEvents { .. }) {
        HOST_SUBSCRIBE_RESPONSE_DEADLINE
    } else if is_native_history_request(request) {
        HOST_NATIVE_HISTORY_RESPONSE_DEADLINE
    } else if is_run_context_source_request(request) {
        HOST_RUN_CONTEXT_SOURCE_RESPONSE_DEADLINE
    } else if is_run_read_request(request)
        || is_node_workspace_read_request(request)
        || is_node_workspace_write_request(request)
    {
        HOST_RUN_READ_RESPONSE_DEADLINE
    } else if is_session_spawn_request(request) {
        HOST_SESSION_SPAWN_RESPONSE_DEADLINE
    } else if is_session_control_request(request) {
        HOST_SESSION_CONTROL_RESPONSE_DEADLINE
    } else if is_session_record_mutation_request(request) {
        HOST_SESSION_RECORD_MUTATION_RESPONSE_DEADLINE
    } else if is_host_directory_browse_request(request) {
        HOST_DIRECTORY_BROWSE_RESPONSE_DEADLINE
    } else if is_resource_mutation_request(request) {
        HOST_RESOURCE_MUTATION_RESPONSE_DEADLINE
    } else {
        HOST_DEADLINE
    }
}

/// Builds the operator-visible session address from a route already sealed
/// live (`adapter.exact_route`) plus the C2-authoritative session address a
/// spawn accepted. Unlike `terminal_session_key` (the read-side sibling that
/// parses a client-supplied address), the route half here is never
/// client-controlled -- it is exactly the route `dispatch_session_spawn`
/// dispatched against.
fn session_address_from_receipt(
    route: &NodeRoute,
    session: &SessionAddress,
) -> HarnessRuntimeSessionAddressV1 {
    HarnessRuntimeSessionAddressV1 {
        node_id: route.node_id.as_str().to_owned(),
        incarnation_id: route.expected_incarnation_id.to_string(),
        workspace_id: session.workspace_id.as_str().to_owned(),
        instance_id: session.session.instance_id.0,
        generation: session.session.generation.0,
    }
}

/// Picks the right unit response for a completed `SessionControlFinished`.
/// The twelve verbs share one C2 relay path and `PendingSessionControl`
/// deliberately only returns `Result<(), _>` (see its doc comment), so the
/// wire `kind` tag in `identity.operation` -- never hand-guessed, read the
/// same way `OperatorRequestLogIdentity::describe` reads it -- is what picks
/// the reply shape back apart.
fn session_control_response(identity: &OperatorRequestLogIdentity) -> HarnessOperatorResponseV1 {
    match identity.operation.as_str() {
        "write-session-input" => HarnessOperatorResponseV1::SessionInputWritten,
        "prompt-session" => HarnessOperatorResponseV1::SessionPrompted,
        "resize-session" => HarnessOperatorResponseV1::SessionResized,
        "stop-session" => HarnessOperatorResponseV1::SessionStopped,
        "control-session" => HarnessOperatorResponseV1::SessionControlled,
        "write-session-bytes" => HarnessOperatorResponseV1::SessionBytesWritten,
        "paste-session" => HarnessOperatorResponseV1::SessionPasted,
        "remove-session" => HarnessOperatorResponseV1::SessionRemoved,
        "resume-session" => HarnessOperatorResponseV1::SessionResumed,
        "resolve-interaction" => HarnessOperatorResponseV1::InteractionResolved,
        "set-session-mode" => HarnessOperatorResponseV1::SessionModeSet,
        "set-session-config-option" => HarnessOperatorResponseV1::SessionConfigOptionSet,
        "set-session-model" => HarnessOperatorResponseV1::SessionModelSet,
        other => {
            tracing::error!(operation = other, "unexpected session control operation label");
            HarnessOperatorResponseV1::SessionInputWritten
        }
    }
}

/// Mints a fresh, host-local `(operation_id, idempotency_ref)` pair for one
/// direct `SpawnSession` dispatch. Unlike `authorize_operator_intent`'s HMAC
/// derivation (deterministic over a client-supplied `request_ref`, so a
/// resubmission replays instead of double-spawning), a direct spawn has no
/// CAS/replay layer to key -- see `HarnessOperatorRequestV1::SpawnSession`'s
/// doc comment and DECISIONS: the TUI sends it once, no auto-retry. `nonce`
/// is a plain per-host monotonic counter (see its call site in the select
/// loop); it only needs to make each dispatch's ids visibly distinct in
/// tracing output, not to be unpredictable -- nothing looks these ids up.
fn mint_session_spawn_ids(
    nonce: u64,
) -> Result<(HarnessOperationId, HarnessIdempotencyRef), HarnessOperatorHostErrorV1> {
    let material = nonce.to_le_bytes();
    let operation_id = session_spawn_nonce_id(
        HarnessOperationId::PREFIX,
        SESSION_SPAWN_OPERATION_ID_DOMAIN,
        &material,
        HarnessOperationId::new,
    )?;
    let idempotency_ref = session_spawn_nonce_id(
        HarnessIdempotencyRef::PREFIX,
        SESSION_SPAWN_IDEMPOTENCY_REF_DOMAIN,
        &material,
        HarnessIdempotencyRef::new,
    )?;
    Ok((operation_id, idempotency_ref))
}

fn session_spawn_nonce_id<T>(
    prefix: &str,
    domain: &[u8],
    material: &[u8],
    constructor: impl FnOnce(String) -> Result<T, gate4agent_harness_protocol::HarnessValidationError>,
) -> Result<T, HarnessOperatorHostErrorV1> {
    let digest = local_hmac_sha256(domain, material).map_err(|cause| {
        tracing::error!(cause = %cause, "session spawn nonce derivation failed");
        HarnessOperatorHostErrorV1::Internal
    })?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        let _ = write!(&mut nonce, "{byte:02x}");
    }
    constructor(format!("{prefix}{nonce}")).map_err(|cause| {
        tracing::error!(cause = %cause, "session spawn nonce identity construction failed");
        HarnessOperatorHostErrorV1::Internal
    })
}

fn run_context_source_run_id(
    request: &HarnessOperatorRequestV1,
) -> Option<&gate4agent_harness_protocol::HarnessRunId> {
    match request {
        HarnessOperatorRequestV1::ObserveRunContextSource { run_id } => Some(run_id),
        _ => None,
    }
}

fn run_read_run_id(request: &HarnessOperatorRequestV1) -> Option<&gate4agent_harness_protocol::HarnessRunId> {
    match request {
        HarnessOperatorRequestV1::InspectRunWorkspace { run_id }
        | HarnessOperatorRequestV1::ReadRunWorkspaceFile { run_id, .. }
        | HarnessOperatorRequestV1::ReadRunGitHistory { run_id, .. }
        | HarnessOperatorRequestV1::ReadRunGitDiff { run_id, .. } => Some(run_id),
        _ => None,
    }
}

fn validate_run_read_completion_origin(
    current: Option<&gate4agent_harness_protocol::HarnessRunV1>,
    prepared: &PreparedRunRead,
) -> Result<(), HarnessOperatorHostErrorV1> {
    let current = current.ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    if current.binding.as_ref() != Some(prepared.binding()) {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    Ok(())
}

fn unobserved_run_context_source(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    feature_state: FeatureObservationStateV1,
) -> HarnessRunContextSourceObservationV1 {
    HarnessRunContextSourceObservationV1 {
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        feature_state,
        message_count: 0,
        message_count_exact: false,
        completed_turn_count: None,
        total_tokens: None,
        observed_at_unix_ms: None,
    }
}

fn exact_context_source_record<'a>(
    runtime_inventory: &'a HarnessRuntimeInventoryCache,
    prepared: &PreparedRunContextSourceObservation,
) -> Result<&'a HarnessRuntimeManagedSessionV1, HarnessOperatorHostErrorV1> {
    let node = runtime_inventory.nodes.get(&prepared.route().node_id)
        .ok_or(HarnessOperatorHostErrorV1::Unavailable)?;
    if node.incarnation_id != prepared.route().expected_incarnation_id.to_string() {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    let mut records = node.inventory.managed_sessions.iter().filter(|record| {
        record.record_id == prepared.record_id().as_str()
            && record.workspace_id == prepared.binding().workspace_id.as_str()
    });
    let record = records.next().ok_or(HarnessOperatorHostErrorV1::Conflict)?;
    if records.next().is_some() {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    let HarnessSessionIdentityV1::Managed { active_session, .. } =
        &prepared.binding().session
    else {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    };
    let active_matches = match (active_session, record.active_binding.as_ref()) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            actual.workspace_id == prepared.binding().workspace_id.as_str()
                && actual.instance_id == expected.instance_id
                && actual.generation == expected.generation
        }
        _ => false,
    };
    if !active_matches {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    Ok(record)
}

fn prepare_run_context_source_observation(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    runtime_inventory: &HarnessRuntimeInventoryCache,
) -> Result<
    Result<PreparedRunContextSourceObservation, HarnessRunContextSourceObservationV1>,
    HarnessOperatorHostErrorV1,
> {
    let mut prepared = PreparedRunContextSourceObservation::from_run(run)
        .map_err(map_run_context_source_error)?;
    let record = exact_context_source_record(runtime_inventory, &prepared)?;
    let route_support = support.get(
        &prepared.route().node_id,
        prepared.route().expected_incarnation_id,
    ).ok_or(HarnessOperatorHostErrorV1::Unavailable)?;
    // A same-incarnation topology notification temporarily marks the route
    // unhealthy while recovery runs, but the exact runtime cache remains the
    // restart authority for this sealed request. Completion still requires a
    // new durable HistorySnapshot after recovery makes the route authoritative.
    if !record.provider_identity_present
        || !route_support.is_some_and(|support| {
            support.events && support.managed_target
        })
    {
        return Ok(Err(unobserved_run_context_source(
            run,
            FeatureObservationStateV1::NotSupportedByObservedSources,
        )));
    }
    let eligible = run.lifecycle == HarnessRunLifecycleV1::Running
        && matches!(
            &prepared.binding().session,
            HarnessSessionIdentityV1::Managed { active_session: Some(_), .. }
        )
        && record.state == HarnessRuntimeManagedStateV1::Live;
    if !eligible {
        return Ok(Err(unobserved_run_context_source(
            run,
            FeatureObservationStateV1::SupportedNotObserved,
        )));
    }
    prepared.set_observed_after_sequence(
        durable_cursor_for(observation, prepared.route()).unwrap_or(0),
    );
    Ok(Ok(prepared))
}

fn latest_matching_context_source_observed_at(
    observation: &ObservationService,
    prepared: &PreparedRunContextSourceObservation,
    projection: &RunContextSourceProjection,
) -> Result<Option<u64>, HarnessOperatorHostErrorV1> {
    let RunContextSourceProjection::Aggregate {
        message_count,
        completed_turn_count,
        total_tokens,
    } = projection else {
        return Ok(None);
    };
    let node_id = gate4agent_observation_api::NodeId::new(
        prepared.route().node_id.as_str(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let record_id = gate4agent_observation_api::SessionRecordId::new(
        prepared.record_id().as_str(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let target = ObservationTarget::Managed { key: ManagedSessionKey {
        node_id,
        incarnation_id: prepared.route().expected_incarnation_id,
        record_id,
    } };
    let Some(session) = observation.projection(&target) else { return Ok(None); };
    Ok(session.timeline.iter().rev().find_map(|entry| {
        if entry.cursor.sequence <= prepared.observed_after_sequence() {
            return None;
        }
        match &entry.kind {
            gate4agent_observation_protocol::ObservationKindV1::HistorySnapshot {
                message_count: observed_messages,
                message_count_exact,
                completed_turn_count: observed_turns,
                total_tokens: observed_tokens,
            } if observed_messages == message_count
                && *message_count_exact
                && observed_turns == completed_turn_count
                && observed_tokens == total_tokens => Some(entry.received_at_ms),
            _ => None,
        }
    }))
}

fn evaluate_pending_run_context_source(
    harness: &HarnessService,
    observation: &mut ObservationService,
    support: &ObservationSupportRegistry,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    pending: &PendingRunContextSourceReply,
) -> Result<Option<HarnessRunContextSourceObservationV1>, HarnessOperatorHostErrorV1> {
    let current = harness.engine().run(pending.prepared.run_id())
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    if current.binding.as_ref() != Some(pending.prepared.binding()) {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    let observed_at_unix_ms = latest_matching_context_source_observed_at(
        observation,
        &pending.prepared,
        &pending.projection,
    )?;
    let Some(observed_at_unix_ms) = observed_at_unix_ms else { return Ok(None); };
    let Some(source) = context_source_option(
        harness,
        observation,
        support,
        runtime_inventory,
        current,
    )? else {
        return Ok(None);
    };
    let RunContextSourceProjection::Aggregate {
        message_count,
        completed_turn_count,
        total_tokens,
    } = &pending.projection else {
        return Ok(None);
    };
    if source.message_count != *message_count
        || !source.message_count_exact
        || source.completed_turn_count != *completed_turn_count
        || source.total_tokens != *total_tokens
    {
        return Ok(None);
    }
    observation.flush().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(Some(HarnessRunContextSourceObservationV1 {
        run_id: current.run_id.clone(),
        run_revision: current.revision,
        feature_state: FeatureObservationStateV1::Observed,
        message_count: *message_count,
        message_count_exact: true,
        completed_turn_count: *completed_turn_count,
        total_tokens: *total_tokens,
        observed_at_unix_ms: Some(observed_at_unix_ms),
    }))
}

fn poll_pending_run_context_sources(
    harness: &HarnessService,
    observation: &mut ObservationService,
    support: &ObservationSupportRegistry,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    pending: &mut Vec<PendingRunContextSourceReply>,
) {
    let now = Instant::now();
    let mut waiting = Vec::with_capacity(pending.len());
    for item in pending.drain(..) {
        let result = evaluate_pending_run_context_source(
            harness,
            observation,
            support,
            runtime_inventory,
            &item,
        );
        match result {
            Ok(Some(observation)) => {
                let _ = item.reply.send(HarnessOperatorReplyV1::Ok {
                    response: HarnessOperatorResponseV1::RunContextSourceObserved(observation),
                });
            }
            Ok(None) if now >= item.deadline => {
                let _ = item.reply.send(HarnessOperatorReplyV1::Error {
                    error: HarnessOperatorHostErrorV1::Unavailable,
                });
            }
            Err(error) => {
                let _ = item.reply.send(HarnessOperatorReplyV1::Error { error });
            }
            Ok(None) => waiting.push(item),
        }
    }
    *pending = waiting;
}

fn ensure_run_context_source_recovery_if_pending(
    pending: &[PendingRunContextSourceReply],
    recovery: &mut ObservationRecoveryRegistry,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    route: &NodeRoute,
    observed_after_sequence: u64,
) {
    if pending.iter().any(|pending| {
        pending.prepared.run_id() == run_id
            && pending.prepared.route() == route
            && pending.prepared.observed_after_sequence() == observed_after_sequence
    }) {
        recovery.ensure_route(route.clone());
    }
}

fn start_delivery_stage_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    lease: crate::c2::PreparedDeliveryStageLease,
) {
    tokio::spawn(async move {
        let result = adapter.stage_compiled_delivery(lease).await;
        let _ = commands.send(HostCommand::DeliveryStageFinished {
            operation_id,
            result,
        }).await;
    });
}

fn start_continuation_export_finish(
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    start: ContextPackExportStart,
) {
    tokio::spawn(async move {
        let result = match start {
            ContextPackExportStart::Enqueued(pending) => pending.finish().await,
            ContextPackExportStart::EnqueuedDurable(pending) => pending.finish().await,
            ContextPackExportStart::NotEnqueued(outcome) => Ok(outcome),
        };
        let _ = commands.send(HostCommand::ContinuationExportFinished {
            operation_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_arm_finish(
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    spec: gate4agent_node_protocol::SpawnSpec,
    profile: SpawnProfileRevisionProof,
    pending: crate::c2::PendingHarnessMcpArm,
) {
    tokio::spawn(async move {
        let result = pending.finish().await;
        let _ = commands.send(HostCommand::HarnessMcpArmFinished {
            operation_id,
            spec,
            profile,
            result,
        }).await;
    });
}

fn start_harness_mcp_activation_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    record_id: SessionRecordId,
    session: SessionAddress,
    attempt_id: u64,
    expected_revision: HarnessRevision,
) {
    let reservation_id = reservation.reservation_id.clone();
    tokio::spawn(async move {
        let result = adapter.activate_harness_mcp_reservation(
            &route,
            reservation,
            record_id,
            session,
        ).await;
        let _ = commands.send(HostCommand::HarnessMcpActivationFinished {
            reservation_id,
            attempt_id,
            expected_revision,
            result,
        }).await;
    });
}

fn start_harness_mcp_abort_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    cleanup: PendingHarnessMcpAbort,
    attempt_id: u64,
) {
    tokio::spawn(async move {
        let result = adapter.abort_harness_mcp_reservation(
            &cleanup.route,
            &cleanup.reservation_id,
            &cleanup.activation_digest,
        ).await;
        let _ = commands.send(HostCommand::HarnessMcpAbortFinished {
            reservation_id: cleanup.reservation_id,
            attempt_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_relay_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    plan: HarnessMcpRelayPlan,
    attempt_id: u64,
) {
    let reservation_id = plan.reservation_id.clone();
    let call_id = plan.call_id.clone();
    tokio::spawn(async move {
        let result = relay_harness_mcp_read_call(&adapter, plan).await;
        let _ = commands.send(HostCommand::HarnessMcpRelayFinished {
            reservation_id,
            call_id,
            attempt_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_reject_worker(
    adapter: HarnessC2Adapter,
    mut rejects: mpsc::Receiver<HarnessMcpRelayPlan>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(plan) = rejects.recv().await {
            let _ = relay_harness_mcp_read_call(&adapter, plan).await;
        }
    })
}

fn start_or_resume_dispatch_job(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    catalogs: &HarnessRuntimeCatalogs,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    intent: HarnessDispatchIntentV1,
) -> Result<Option<ActiveDispatchJob>, HarnessRuntimeError> {
    // Recomputed here (not threaded in from the caller) so this always
    // resolves the durable scheduled ref against the launch catalog
    // current as of this call -- see `effective_launch_catalog`.
    let (launch, _truncated) = effective_launch_catalog(&catalogs.launch, runtime_inventory);
    let scheduled = harness.scheduled_launch(&intent.operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation("operation has no scheduled launch"))?;
    let plan = launch.resolve_scheduled(scheduled)
        .map_err(|_| HarnessRuntimeError::DispatchPreparation(
            "scheduled launch does not resolve against the launch catalog",
        ))?
        .clone();
    let has_delivery = harness.engine().delivery_for_run(&intent.run_id).is_some();
    let has_continuation = harness.engine().continuation_for_run(&intent.run_id).is_some();
    if plan.is_ordinary_dispatch() && !has_delivery && !has_continuation {
        start_dispatch_preflight(adapter, commands, intent.clone())?;
        return Ok(Some(ActiveDispatchJob::new(
            intent.operation_id,
            CoordinatorDispatchPhase::Preflight,
            plan,
        )));
    }
    if !has_delivery && !has_continuation {
        harness.prepare_scheduled_specialized_authorities(
            &launch,
            &catalogs.delivery,
            &intent.operation_id,
            unix_time_ms(),
        )?;
    }
    if let Some(delivery) = harness.engine().delivery_for_run(&intent.run_id) {
        if delivery_needs_staging(delivery.state)? {
            let delivery_ref = delivery.delivery_ref.clone();
            let bundle_id = SpawnBundleId::new(delivery.bundle.bundle_id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "delivery bundle id is not a valid spawn bundle id",
                ))?;
            let compiled = catalogs.delivery.get(&bundle_id)
                .ok_or(HarnessRuntimeError::DispatchPreparation(
                    "delivery bundle id is missing from the delivery catalog",
                ))?
                .clone();
            let node_id = NodeId::new(intent.intent.node_id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                    "dispatch intent node id is not a valid node id",
                ))?;
            let route = adapter.exact_route(&node_id)?;
            let operation_id = intent.operation_id.clone();
            let lease = harness.issue_delivery_staging_lease(
                &delivery_ref,
                route,
                compiled,
            )?;
            start_delivery_stage_finish(
                adapter.clone(),
                commands.clone(),
                operation_id.clone(),
                lease,
            );
            return Ok(Some(ActiveDispatchJob::new(
                operation_id,
                CoordinatorDispatchPhase::Delivery,
                plan,
            )));
        }
    }
    if let Some(continuation) = harness.engine().continuation_for_run(&intent.run_id).cloned() {
        match continuation_resume_action(continuation.state) {
            ContinuationResumeAction::BeginExport => {
                let prepared = harness.begin_continuation_export(
                    &continuation.continuation_ref,
                    continuation.revision,
                    unix_time_ms(),
                )?;
                let start = match adapter.start_context_pack_export(prepared) {
                    Ok(start) => start,
                    Err(_) => {
                        let exporting = harness.engine().continuation_for_run(&intent.run_id)
                            .ok_or(HarnessRuntimeError::DispatchPreparation(
                                "run has no continuation record",
                            ))?
                            .clone();
                        harness.recover_exporting_continuation_outcome_unknown(
                            &exporting.continuation_ref,
                            exporting.revision,
                            unix_time_ms(),
                        )?;
                        apply_pre_dispatch_result(
                            harness,
                            &intent.operation_id,
                            CoordinatorPreDispatchResult::OutcomeUnknown,
                            unix_time_ms(),
                        )?;
                        return Ok(None);
                    }
                };
                let operation_id = intent.operation_id.clone();
                start_continuation_export_finish(
                    commands.clone(),
                    operation_id.clone(),
                    start,
                );
                return Ok(Some(ActiveDispatchJob::new(
                    operation_id,
                    CoordinatorDispatchPhase::Continuation,
                    plan,
                )));
            }
            ContinuationResumeAction::RecoverOutcomeUnknown => {
                harness.recover_exporting_continuation_outcome_unknown(
                    &continuation.continuation_ref,
                    continuation.revision,
                    unix_time_ms(),
                )?;
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::OutcomeUnknown,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::FinishOutcomeUnknown => {
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::OutcomeUnknown,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::FinishFailed => {
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::Failed,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::Preflight => {}
            ContinuationResumeAction::Reject => {
                return Err(HarnessRuntimeError::DispatchPreparation(
                    "continuation is already Bound",
                ));
            }
        }
    }
    start_dispatch_preflight(adapter, commands, intent.clone())?;
    Ok(Some(ActiveDispatchJob::new(
        intent.operation_id,
        CoordinatorDispatchPhase::Preflight,
        plan,
    )))
}

fn dispatch_start_pre_dispatch_result(
    error: &HarnessRuntimeError,
) -> CoordinatorPreDispatchResult {
    match error {
        HarnessRuntimeError::C2(
            HarnessC2Error::DeliveryTransport(_)
            | HarnessC2Error::ContextExportTransport(_),
        ) => CoordinatorPreDispatchResult::OutcomeUnknown,
        _ => {
            note_terminal_pre_dispatch("dispatch-start", error);
            CoordinatorPreDispatchResult::Failed
        }
    }
}

fn start_or_terminalize_dispatch_job(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    catalogs: &HarnessRuntimeCatalogs,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    pending_harness_mcp_aborts: &mut BTreeMap<
        HarnessMcpReservationId,
        PendingHarnessMcpAbort,
    >,
    intent: HarnessDispatchIntentV1,
) -> Result<Option<ActiveDispatchJob>, HarnessRuntimeError> {
    let operation_id = intent.operation_id.clone();
    match start_or_resume_dispatch_job(harness, adapter, commands, catalogs, runtime_inventory, intent) {
        Ok(job) => Ok(job),
        Err(error) => {
            let operation_state = harness.engine().operation(&operation_id)
                .map(|operation| operation.state);
            let reservation_id = match operation_state {
                Some(HarnessOperationStateV1::Prepared) => apply_pre_dispatch_result(
                    harness,
                    &operation_id,
                    dispatch_start_pre_dispatch_result(&error),
                    unix_time_ms(),
                )?,
                Some(HarnessOperationStateV1::Dispatching) => apply_spawn_result(
                    harness,
                    &effective_launch_catalog(&catalogs.launch, runtime_inventory).0,
                    &operation_id,
                    match dispatch_start_pre_dispatch_result(&error) {
                        CoordinatorPreDispatchResult::Failed => CoordinatorSpawnResult::Failed,
                        CoordinatorPreDispatchResult::OutcomeUnknown => {
                            CoordinatorSpawnResult::OutcomeUnknown(Some(error.to_string()))
                        }
                    },
                    unix_time_ms(),
                )?,
                Some(
                    HarnessOperationStateV1::Succeeded
                    | HarnessOperationStateV1::Failed
                    | HarnessOperationStateV1::OutcomeUnknown
                    | HarnessOperationStateV1::Reconciled,
                ) => None,
                None => return Err(HarnessRuntimeError::DispatchPreparation("operation is missing")),
            };
            if let Some(reservation_id) = reservation_id {
                if let Some(cleanup) = harness
                    .harness_mcp_reservation(&reservation_id)
                    .and_then(pending_harness_mcp_abort)
                {
                    pending_harness_mcp_aborts
                        .entry(reservation_id)
                        .or_insert(cleanup);
                }
            }
            Ok(None)
        }
    }
}

fn start_dispatch_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    route: NodeRoute,
    pending: PendingCoordinatorSpawn,
) {
    tokio::spawn(async move {
        let result = match pending {
            PendingCoordinatorSpawn::Direct(pending) => match pending.finish().await {
                Ok(SpawnDispatchOutcome::Accepted(accepted)) => {
                    match adapter.resolve_accepted_receipt(&route, &accepted).await {
                        Ok(proof) => CoordinatorSpawnResult::Accepted(proof),
                        Err(_) => CoordinatorSpawnResult::OutcomeUnknown(None),
                    }
                }
                Ok(SpawnDispatchOutcome::Rejected { code }) => {
                    CoordinatorSpawnResult::Rejected(code)
                }
                Ok(SpawnDispatchOutcome::OutcomeUnknown { .. }) | Err(_) => {
                    CoordinatorSpawnResult::OutcomeUnknown(None)
                }
            },
            PendingCoordinatorSpawn::Managed(pending) => match pending.finish().await {
                Ok(ManagedWorktreeSpawnDispatchOutcome::Accepted(accepted)) => {
                    match adapter.resolve_managed_accepted_receipt(&route, &accepted).await {
                        Ok(proof) => CoordinatorSpawnResult::Accepted(proof),
                        Err(_) => CoordinatorSpawnResult::OutcomeUnknown(None),
                    }
                }
                Ok(ManagedWorktreeSpawnDispatchOutcome::Rejected { code }) => {
                    CoordinatorSpawnResult::Rejected(code)
                }
                Ok(ManagedWorktreeSpawnDispatchOutcome::OutcomeUnknown { .. }) | Err(_) => {
                    CoordinatorSpawnResult::OutcomeUnknown(None)
                }
            },
        };
        let _ = commands.send(HostCommand::DispatchFinished { operation_id, result }).await;
    });
}

pub async fn start_harness_host(
    harness: HarnessService,
    observation: ObservationService,
    adapter: HarnessC2Adapter,
    events: HarnessC2EventReceiver,
    bind: SocketAddr,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    start_harness_host_with_operator(
        harness,
        observation,
        adapter,
        events,
        bind,
        None,
    ).await
}

pub async fn start_harness_host_with_operator(
    harness: HarnessService,
    observation: ObservationService,
    adapter: HarnessC2Adapter,
    events: HarnessC2EventReceiver,
    bind: SocketAddr,
    operator_credential: Option<HarnessOperatorCredential>,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        bind,
        operator_credential,
        HarnessRuntimeCatalogs::default(),
    ).await
}

pub async fn start_harness_host_with_operator_and_catalogs(
    mut harness: HarnessService,
    mut observation: ObservationService,
    adapter: HarnessC2Adapter,
    mut events: HarnessC2EventReceiver,
    bind: SocketAddr,
    operator_credential: Option<HarnessOperatorCredential>,
    catalogs: HarnessRuntimeCatalogs,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    catalogs.launch.validate_delivery_catalog(&catalogs.delivery)
        .map_err(|_| HarnessRuntimeError::LaunchCatalog)?;
    if bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(HarnessRuntimeError::NonLoopbackBind);
    }
    let listener = TcpListener::bind(bind).await.map_err(|_| HarnessRuntimeError::BindFailed)?;
    let endpoint = HarnessHostEndpoint(
        listener.local_addr().map_err(|_| HarnessRuntimeError::BindFailed)?,
    );
    let mut support = ObservationSupportRegistry::default();
    let mut runtime_inventory = HarnessRuntimeInventoryCache::default();
    let mut terminal_buffers = TerminalBufferRegistry::default();
    let mut topology = adapter.topology_receiver();
    recover_all_routes(
        &adapter,
        &mut harness,
        &mut observation,
        &mut support,
        &mut runtime_inventory,
    ).await?;
    let mut pending_harness_mcp_aborts = durable_harness_mcp_abort_cleanup(&harness);
    let harness_mcp_actions = prepare_harness_mcp_reconcile(
        &mut harness,
        &adapter,
        &observation,
        &support,
    )?;
    execute_harness_mcp_reconcile(
        &mut harness,
        &adapter,
        harness_mcp_actions,
        &mut pending_harness_mcp_aborts,
    ).await?;
    retry_pending_harness_mcp_aborts(
        &adapter,
        &mut pending_harness_mcp_aborts,
    ).await;
    let authority = CredentialAuthority::new()?;
    let operator_authority = operator_credential
        .map(HarnessOperatorCredentialAuthority::new)
        .transpose()?;
    let (commands, mut command_rx) = mpsc::channel(HOST_COMMAND_CAPACITY);
    let handle = HarnessHostHandle { endpoint, commands: commands.clone() };
    let connections = Arc::new(Semaphore::new(HOST_CONNECTION_LIMIT));
    let subscriber_connections = Arc::new(Semaphore::new(HOST_SUBSCRIBER_LIMIT));
    let terminal_subscriber_connections = Arc::new(Semaphore::new(HOST_TERMINAL_SUBSCRIBER_LIMIT));
    let agent_stream_subscriber_connections =
        Arc::new(Semaphore::new(HOST_AGENT_STREAM_SUBSCRIBER_LIMIT));
    let task = tokio::spawn(async move {
        // The loop below has no `break`: its only exits are the early
        // `return Ok(())`/`return Err(_)` statements inside the `select!`
        // arms (`HostCommand::Shutdown`, `HostCommand::None` on a closed
        // command channel, `listener.accept()` failing, and every other
        // `?`-propagated error from live-event/dispatch processing). Every
        // one of those return sites drops this task's `listener` --
        // silently closing the operator wire's bound port -- while the
        // rest of the process (this task's own caller in `main`, which
        // only awaits `ctrl_c()` until shutdown, plus every other spawned
        // task) keeps running unaware. Wrapping the whole body in its own
        // `async move` here, rather than annotating every return site
        // individually, guarantees exactly one place -- this one -- ever
        // needs to say why the loop stopped serving, no matter which arm
        // did it.
        let result: Result<(), HarnessRuntimeError> = async move {
        let mut active_dispatch = None;
        let mut subscribers = SubscriberRegistry::default();
        let mut terminal_subscribers = TerminalSubscriberRegistry::default();
        let mut agent_stream_subscribers = AgentStreamSubscriberRegistry::default();
        let mut harness_mcp_workers = HarnessMcpWorkerRegistry::default();
        let mut native_history_workers = NativeHistoryWorkerRegistry::default();
        let mut run_read_workers = RunReadWorkerRegistry::default();
        let mut node_workspace_read_workers = NodeWorkspaceReadWorkerRegistry::default();
        let mut node_workspace_write_workers = NodeWorkspaceWriteWorkerRegistry::default();
        let mut session_spawn_workers = SessionSpawnWorkerRegistry::default();
        let mut session_control_workers = SessionControlWorkerRegistry::default();
        let mut session_record_mutation_workers = SessionRecordMutationWorkerRegistry::default();
        let mut host_directory_browse_workers = HostDirectoryBrowseWorkerRegistry::default();
        let mut resource_mutation_workers = ResourceMutationWorkerRegistry::default();
        // Host-local nonce for `mint_session_spawn_ids` -- see its doc
        // comment for why this only needs to be distinct, not unpredictable.
        let mut session_spawn_nonce: u64 = 0;
        let mut run_git_facts_workers = RunGitFactsWorkerRegistry::default();
        let mut run_context_source_workers = RunContextSourceWorkerRegistry::default();
        let mut pending_run_context_sources = Vec::new();
        let (harness_mcp_rejects, harness_mcp_reject_rx) = mpsc::channel(
            MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
        );
        let _harness_mcp_reject_worker = start_harness_mcp_reject_worker(
            adapter.clone(),
            harness_mcp_reject_rx,
        );
        let stranded_dispatches = harness.engine().operations()
            .filter(|operation| {
                operation.state == HarnessOperationStateV1::Dispatching
                    && harness.scheduled_launch(&operation.operation_id).is_some()
            })
            .map(|operation| operation.operation_id.clone())
            .collect::<Vec<_>>();
        for operation_id in stranded_dispatches {
            if let Some(reservation_id) = apply_spawn_result(
                &mut harness,
                &effective_launch_catalog(&catalogs.launch, &runtime_inventory).0,
                &operation_id,
                CoordinatorSpawnResult::OutcomeUnknown(Some(
                    "operation was still Dispatching at harness restart; reply outcome was never learned".to_owned(),
                )),
                unix_time_ms(),
            )? {
                if let Some(cleanup) = harness
                    .harness_mcp_reservation(&reservation_id)
                    .and_then(pending_harness_mcp_abort)
                {
                    pending_harness_mcp_aborts
                        .entry(reservation_id)
                        .or_insert(cleanup);
                }
            }
        }
        if let Some(intent) = harness.pending_scheduled_dispatch()? {
            active_dispatch = start_or_terminalize_dispatch_job(
                &mut harness,
                &adapter,
                &commands,
                &catalogs,
                &runtime_inventory,
                &mut pending_harness_mcp_aborts,
                intent,
            )?;
        }
        let mut events_open = true;
        let mut topology_open = true;
        let mut recovery_retry = interval_at(
            Instant::now() + OBSERVATION_RECOVERY_RETRY,
            OBSERVATION_RECOVERY_RETRY,
        );
        recovery_retry.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut run_git_facts_sweep = interval_at(
            Instant::now() + RUN_GIT_FACTS_SWEEP_PERIOD,
            RUN_GIT_FACTS_SWEEP_PERIOD,
        );
        run_git_facts_sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut run_context_source_poll = interval_at(
            Instant::now() + RUN_CONTEXT_SOURCE_POLL_INTERVAL,
            RUN_CONTEXT_SOURCE_POLL_INTERVAL,
        );
        run_context_source_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut subscriber_keepalive = interval_at(
            Instant::now() + HOST_SUBSCRIBER_KEEPALIVE_INTERVAL,
            HOST_SUBSCRIBER_KEEPALIVE_INTERVAL,
        );
        subscriber_keepalive.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut observation_recovery = ObservationRecoveryRegistry::default();
        loop {
            tokio::select! {
                command = command_rx.recv() => {
                    match command {
                        Some(HostCommand::Mint {
                            binding,
                            issued_at_unix_ms,
                            expires_at_unix_ms,
                            reply,
                        }) => {
                            let result = ensure_current_topology_binding(&adapter, &binding)
                                .and_then(|_| verify_observation_credential_binding(
                                    &observation,
                                    &support,
                                    &binding,
                                ).map_err(|_| HarnessRuntimeError::CredentialBinding))
                                .and_then(|_| authority.mint(
                                    harness.engine(),
                                    binding,
                                    issued_at_unix_ms,
                                    expires_at_unix_ms,
                                ).map_err(HarnessRuntimeError::Credential));
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::Read { envelope, reply }) => {
                            let response = authority.verify(
                                harness.engine(),
                                &envelope.credential,
                                unix_time_ms(),
                            ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)
                                .and_then(|claims| {
                                    ensure_current_topology_binding(
                                        &adapter,
                                        &claims.binding,
                                    ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
                                    verify_observation_credential_binding(
                                        &observation,
                                        &support,
                                        &claims.binding,
                                    )?;
                                    let response = execute_read(
                                        &harness,
                                        &observation,
                                        &support,
                                        &claims,
                                        envelope.request,
                                    )?;
                                    response.validate()
                                        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
                                    Ok(response)
                                });
                            let reply_value = match response {
                                Ok(response) => HarnessReadReplyV1::Ok { response },
                                Err(error) => HarnessReadReplyV1::Error { error },
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::Operator { request, reply, cancel }) => {
                            if is_run_context_source_request(&request) {
                                let prepared = run_context_source_run_id(&request)
                                    .ok_or(HarnessOperatorHostErrorV1::InvalidRequest)
                                    .and_then(|run_id| {
                                        harness.engine().run(run_id).cloned()
                                            .ok_or(HarnessOperatorHostErrorV1::NotFound)
                                    })
                                    .and_then(|run| {
                                        prepare_run_context_source_observation(
                                            &run,
                                            &observation,
                                            &support,
                                            &runtime_inventory,
                                        )
                                    });
                                let prepared = match prepared {
                                    Ok(Ok(prepared)) => prepared,
                                    Ok(Err(observation)) => {
                                        let _ = reply.send(HarnessOperatorReplyV1::Ok {
                                            response: HarnessOperatorResponseV1::RunContextSourceObserved(
                                                observation,
                                            ),
                                        });
                                        continue;
                                    }
                                    Err(error) => {
                                        let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                        continue;
                                    }
                                };
                                if !run_context_source_workers.try_start() {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_run_context_source_observation(
                                    prepared,
                                ) {
                                    Ok(pending) => start_run_context_source_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                    ),
                                    Err(error) => {
                                        run_context_source_workers.finish();
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_run_context_source_error(error),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_run_read_request(&request) {
                                let prepared = run_read_run_id(&request)
                                    .ok_or(HarnessOperatorHostErrorV1::InvalidRequest)
                                    .and_then(|run_id| {
                                        harness.engine().run(run_id).cloned()
                                            .ok_or(HarnessOperatorHostErrorV1::NotFound)
                                    })
                                    .and_then(|run| {
                                        PreparedRunRead::from_operator_request(&run, request)
                                            .map_err(map_run_read_error)
                                    });
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(error) => {
                                        let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                        continue;
                                    }
                                };
                                if !run_read_workers.try_start() {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_run_read(prepared) {
                                    Ok(pending) => start_run_read_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                    ),
                                    Err(error) => {
                                        run_read_workers.finish();
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_run_read_error(error),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_node_workspace_read_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedNodeWorkspaceRead::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            cause = %cause,
                                            "node workspace read request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_node_workspace_read_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !node_workspace_read_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        limit = NODE_WORKSPACE_READ_WORKERS_MAX,
                                        "node workspace read rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_node_workspace_read(prepared) {
                                    Ok(pending) => start_node_workspace_read_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        node_workspace_read_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            cause = %cause,
                                            "node workspace read rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_node_workspace_read_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_node_workspace_write_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedNodeWorkspaceWrite::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "node workspace write request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_node_workspace_write_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !node_workspace_write_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        path = identity.path(),
                                        limit = NODE_WORKSPACE_WRITE_WORKERS_MAX,
                                        "node workspace write rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_node_workspace_write(prepared) {
                                    Ok(pending) => start_node_workspace_write_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        node_workspace_write_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "node workspace write rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_node_workspace_write_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_session_spawn_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedSessionSpawn::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            provider = identity.provider(),
                                            provider_profile = identity.provider_profile(),
                                            cause = %cause,
                                            "session spawn request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_session_spawn_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !session_spawn_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        provider = identity.provider(),
                                        provider_profile = identity.provider_profile(),
                                        limit = SESSION_SPAWN_WORKERS_MAX,
                                        "session spawn rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                session_spawn_nonce = session_spawn_nonce.wrapping_add(1).max(1);
                                let (operation_id, idempotency_ref) = match mint_session_spawn_ids(
                                    session_spawn_nonce,
                                ) {
                                    Ok(ids) => ids,
                                    Err(error) => {
                                        session_spawn_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            provider = identity.provider(),
                                            provider_profile = identity.provider_profile(),
                                            "session spawn rejected: local operation identity minting failed",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                        continue;
                                    }
                                };
                                start_session_spawn_worker(
                                    adapter.clone(),
                                    prepared,
                                    operation_id,
                                    idempotency_ref,
                                    commands.clone(),
                                    reply,
                                    identity,
                                    cancel,
                                );
                                continue;
                            }
                            if is_session_control_request(&request) {
                                // `PromptSession`'s dispatch-time PTY refusal
                                // (see `prompt_session_pty_refusal`'s doc
                                // comment): checked here, against the
                                // request's own `session` field, before it
                                // is moved into `PreparedSessionControl::
                                // from_operator_request` below -- `runtime_
                                // inventory` is this select loop's own
                                // cached projection, not something
                                // `PreparedSessionControl` (synchronous, no
                                // cache in scope) could consult itself.
                                if let HarnessOperatorRequestV1::PromptSession { session, .. } =
                                    &request
                                {
                                    if let Some(error) =
                                        prompt_session_pty_refusal(&runtime_inventory, session)
                                    {
                                        tracing::warn!(
                                            node_id = session.node_id.as_str(),
                                            workspace_id = session.workspace_id.as_str(),
                                            session_id = %format!(
                                                "{}/{}", session.instance_id, session.generation,
                                            ),
                                            cause = ?error,
                                            "prompt-session rejected before C2 dispatch: \
                                             target session is PTY-transport",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                        continue;
                                    }
                                }
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedSessionControl::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            session_id = identity.session_id(),
                                            cause = %cause,
                                            "session control request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_session_control_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !session_control_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        session_id = identity.session_id(),
                                        limit = SESSION_CONTROL_WORKERS_MAX,
                                        "session control rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_session_control(prepared) {
                                    Ok(pending) => start_session_control_worker(
                                        adapter.clone(),
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        session_control_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            session_id = identity.session_id(),
                                            cause = %cause,
                                            "session control rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_session_control_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_session_record_mutation_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedSessionRecordMutation::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            session_id = identity.session_id(),
                                            cause = %cause,
                                            "session record mutation request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_session_record_mutation_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !session_record_mutation_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        session_id = identity.session_id(),
                                        limit = SESSION_RECORD_MUTATION_WORKERS_MAX,
                                        "session record mutation rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_session_record_mutation(prepared) {
                                    Ok(pending) => start_session_record_mutation_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        session_record_mutation_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            session_id = identity.session_id(),
                                            cause = %cause,
                                            "session record mutation rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_session_record_mutation_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_host_directory_browse_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedHostDirectoryBrowse::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "host directory browse request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_host_directory_browse_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !host_directory_browse_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        path = identity.path(),
                                        limit = HOST_DIRECTORY_BROWSE_WORKERS_MAX,
                                        "host directory browse rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_host_directory_browse(prepared) {
                                    Ok(pending) => start_host_directory_browse_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        host_directory_browse_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "host directory browse rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_host_directory_browse_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_resource_mutation_request(&request) {
                                let identity = OperatorRequestLogIdentity::describe(&request);
                                let prepared = PreparedResourceMutation::from_operator_request(
                                    &adapter,
                                    request,
                                );
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(cause) => {
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            source_workspace_id = identity.source_workspace_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "resource mutation request rejected before C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_resource_mutation_error(cause),
                                        });
                                        continue;
                                    }
                                };
                                if !resource_mutation_workers.try_start() {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        source_workspace_id = identity.source_workspace_id(),
                                        path = identity.path(),
                                        limit = RESOURCE_MUTATION_WORKERS_MAX,
                                        "resource mutation rejected: harness-side worker capacity is busy",
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_resource_mutation(prepared) {
                                    Ok(pending) => start_resource_mutation_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                        identity,
                                        cancel,
                                    ),
                                    Err(cause) => {
                                        resource_mutation_workers.finish();
                                        tracing::warn!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            source_workspace_id = identity.source_workspace_id(),
                                            path = identity.path(),
                                            cause = %cause,
                                            "resource mutation rejected: could not start the C2 dispatch",
                                        );
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_resource_mutation_error(cause),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_native_history_request(&request) {
                                if !native_history_workers.try_start() {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_native_history_request(request) {
                                    Ok(pending) => start_native_history_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                    ),
                                    Err(error) => {
                                        native_history_workers.finish();
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_native_history_error(error),
                                        });
                                    }
                                }
                                continue;
                            }
                            // Captured before `request` moves into
                            // `execute_operator_request` below -- see
                            // `operator_mutation_task_id`'s doc comment for
                            // why this (not the response) is the right
                            // source for every mutation variant except
                            // `ScheduleNext`.
                            let mutation_task_id = operator_mutation_task_id(&request);
                            let response = execute_operator_request(
                                &mut harness,
                                &observation,
                                &support,
                                &catalogs.launch,
                                &catalogs.delivery,
                                &runtime_inventory,
                                &terminal_buffers,
                                request,
                            );
                            let scheduled_dispatch = response.as_ref().ok()
                                .and_then(scheduled_dispatch_from_operator_response);
                            if response.is_ok() {
                                if let Some(task_id) = &mutation_task_id {
                                    notify_task_changed(&mut subscribers, &harness, task_id);
                                }
                                if let Some(dispatch) = &scheduled_dispatch {
                                    notify_task_changed(&mut subscribers, &harness, &dispatch.task_id);
                                    notify_run_changed(&mut subscribers, &harness, &dispatch.run_id);
                                    log_if_derived_launch_plan_used(
                                        &harness,
                                        &catalogs.launch,
                                        &runtime_inventory,
                                        &dispatch.operation_id,
                                    );
                                }
                            }
                            let reply_value = match response {
                                Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                Err(error) => HarnessOperatorReplyV1::Error { error },
                            };
                            let _ = reply.send(reply_value);
                            if active_dispatch.is_none() {
                                if let Some(intent) = scheduled_dispatch {
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &runtime_inventory,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                            }
                        }
                        Some(HostCommand::ApplyHarnessMutation { mutation, reply }) => {
                            let prior = non_revoked_harness_mcp_abort_cleanup(&harness);
                            // Captured before `mutation` moves into
                            // `harness.apply` below: every `HarnessMutationV1`
                            // variant carries its own `HarnessOperationV1`,
                            // which already names the task/run it touches.
                            let touched_task_id = mutation.operation().task_id.clone();
                            let touched_run_id = mutation.operation().run_id.clone();
                            let result = harness.apply(mutation).map_err(HarnessRuntimeError::Harness);
                            if result.is_ok() {
                                enqueue_newly_revoked_harness_mcp_aborts(
                                    &harness,
                                    prior,
                                    &mut pending_harness_mcp_aborts,
                                );
                                start_pending_harness_mcp_abort_workers(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &mut pending_harness_mcp_aborts,
                                );
                                if let Some(task_id) = &touched_task_id {
                                    notify_task_changed(&mut subscribers, &harness, task_id);
                                }
                                if let Some(run_id) = &touched_run_id {
                                    notify_run_changed(&mut subscribers, &harness, run_id);
                                }
                            }
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::ActivateHarnessMcp { reservation_id, reply }) => {
                            let authority = harness.validate_activatable_harness_mcp_authority(
                                &reservation_id,
                                unix_time_ms(),
                            ).map_err(|_| HarnessRuntimeError::HarnessMcpAuthority);
                            match authority {
                                Ok((reservation, record_id, session)) => {
                                    let route = reservation_route(&reservation)?;
                                    if let Err(reply) = schedule_harness_mcp_activation(
                                        &adapter,
                                        &commands,
                                        &mut harness_mcp_workers,
                                        &pending_harness_mcp_aborts,
                                        route,
                                        reservation,
                                        record_id,
                                        session,
                                        unix_time_ms(),
                                        Some(reply),
                                    ) {
                                        if let Some(reply) = reply {
                                            let _ = reply.send(Err(
                                                HarnessRuntimeError::HarnessMcpAuthority,
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        Some(HostCommand::RevokeHarnessMcp { reservation_id, reply }) => {
                            let cleanup = harness.harness_mcp_reservation(&reservation_id)
                                .and_then(pending_harness_mcp_abort)
                                .ok_or(HarnessRuntimeError::HarnessMcpAuthority);
                            let result = match cleanup {
                                Ok(cleanup) => harness.revoke_harness_mcp_reservation(
                                    &reservation_id,
                                    unix_time_ms(),
                                ).map_err(HarnessRuntimeError::Harness)
                                    .map(|()| {
                                        pending_harness_mcp_aborts
                                            .entry(reservation_id.clone())
                                            .or_insert(cleanup);
                                    }),
                                Err(error) => Err(error),
                            };
                            if result.is_ok() {
                                start_pending_harness_mcp_abort_workers(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &mut pending_harness_mcp_aborts,
                                );
                            }
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::DispatchPreflightFinished { intent, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(
                                    &intent.operation_id,
                                    CoordinatorDispatchPhase::Preflight,
                                )
                            }) {
                                continue;
                            }
                            // `intent.operation_id`/`.task_id`/`.run_id`
                            // partially move out of `intent` on some branches
                            // below (e.g. `ActiveDispatchJob::new`), so these
                            // notification ids are captured up front rather
                            // than re-read from `intent` at each exit point.
                            let notify_task_id = intent.task_id.clone();
                            let notify_run_id = intent.run_id.clone();
                            let profile = match result {
                                Ok(profile) => profile,
                                Err(error) => {
                                    active_dispatch = None;
                                    if let Some(reservation_id) = apply_pre_dispatch_result(
                                        &mut harness,
                                        &intent.operation_id,
                                        preflight_pre_dispatch_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                    notify_task_changed(&mut subscribers, &harness, &notify_task_id);
                                    notify_run_changed(&mut subscribers, &harness, &notify_run_id);
                                    continue;
                                }
                            };
                            // Computed once for this whole closure -- reused
                            // below by `issue_spawn_lease` too -- so `plan`
                            // and that later resolve agree on the exact same
                            // launch catalog snapshot (see
                            // `effective_launch_catalog`).
                            let (effective_launch, _truncated) = effective_launch_catalog(
                                &catalogs.launch,
                                &runtime_inventory,
                            );
                            let preparation = (|| -> Result<_, HarnessRuntimeError> {
                            let run = harness.engine().run(&intent.run_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation("run is missing"))?.clone();
                            let operation = harness.engine().operation(&intent.operation_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation("operation is missing"))?.clone();
                            let task = harness.engine().task(&intent.task_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation("task is missing"))?.clone();
                            let scheduled = harness.scheduled_launch(&intent.operation_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation(
                                    "operation has no scheduled launch",
                                ))?.clone();
                            let plan = effective_launch.resolve_scheduled(&scheduled)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "scheduled launch does not resolve against the launch catalog",
                                ))?;
                            let issued_dispatch = (
                                matches!(intent.intent.worktree, HarnessWorktreeIntentV1::ManagedProfile { .. })
                                    || harness.engine().delivery_for_run(&intent.run_id).is_some()
                                    || harness.engine().continuation_for_run(&intent.run_id).is_some()
                            ).then(|| {
                                    let mut ordinary = intent.clone();
                                    ordinary.intent.worktree = HarnessWorktreeIntentV1::Existing;
                                    ordinary.intent.delivery_bundle = None;
                                    ordinary.intent.continuation = None;
                                    ordinary
                                });
                            // The intent above is reduced to what the node
                            // is actually asked to run, so the plan it is
                            // validated against has to be reduced the same
                            // way -- see `HarnessLaunchPlanV1::issued_
                            // view`. The specialized fields come back as
                            // node overrides in `specialized_spawn_spec`
                            // immediately below.
                            let issued_plan = issued_dispatch.as_ref().map(|_| plan.issued_view());
                            let spec = issued_plan.as_ref().unwrap_or(plan).spawn_spec(
                                issued_dispatch.as_ref().unwrap_or(&intent),
                                &task,
                                profile.revision().clone(),
                            )
                                .map_err(|error| HarnessRuntimeError::DispatchPreparation({
                                    // `DispatchPreparation` carries only a
                                    // static reason, so the dispatch
                                    // error's own detail survives in the
                                    // log -- and it must, since
                                    // `spawn_spec` refuses for a dozen
                                    // different reasons that are not
                                    // interchangeable.
                                    tracing::warn!(
                                        ?error,
                                        "launch plan refused to produce a spawn spec for the dispatch intent",
                                    );
                                    "dispatch intent fails to produce a spawn spec from the launch plan"
                                }
                                ))?;
                            let spec = specialized_spawn_spec(
                                &harness,
                                plan,
                                &intent.run_id,
                                spec,
                            )?;
                            let spec = profile.bind_spec(spec)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "spawn spec fails to bind to the resolved spawn profile revision",
                                ))?;
                            let fingerprint = crate::c2::spawn_spec_fingerprint(&spec)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "spawn spec fingerprint failed to derive",
                                ))?;
                            let now = unix_time_ms();
                            let route = profile.route().clone();
                            let context = crate::HarnessDispatchContextV1 {
                                operation_id: intent.operation_id.clone(),
                                node_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    route.node_id.as_str(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "profile route node id is not a valid harness selector",
                                ))?,
                                node_incarnation_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    route.expected_incarnation_id.to_string(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "profile route incarnation id is not a valid harness selector",
                                ))?,
                                workspace_id: intent.intent.workspace_id.clone(),
                                provider_profile: intent.intent.provider_profile.clone(),
                                expected_provider: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    plan.provider.as_str(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "launch plan provider is not a valid harness selector",
                                ))?,
                                mode: intent.intent.mode,
                                baseline_record_ids: Vec::new(),
                                spawn_spec_fingerprint: fingerprint,
                                dispatched_at_unix_ms: now,
                                idempotency_ref: intent.idempotency_ref.clone(),
                                managed_worktree_binding: None,
                            };
                            let mut dispatching_run = run.clone();
                            dispatching_run.revision = next_runtime_revision(run.revision)?;
                            dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
                            dispatching_run.updated_at_unix_ms = now;
                            let mut dispatching_operation = operation.clone();
                            dispatching_operation.revision = next_runtime_revision(
                                operation.revision,
                            )?;
                            dispatching_operation.state = HarnessOperationStateV1::Dispatching;
                            dispatching_operation.updated_at_unix_ms = now;
                            dispatching_operation.dispatched_at_unix_ms = Some(now);
                            if plan.harness_mcp
                                == crate::dispatch::HarnessMcpPolicyV1::GrantBound
                            {
                                let ids = deterministic_dispatch_ids(
                                    &intent.operation_id,
                                    plan,
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation(
                                    "operation id or launch plan fails deterministic dispatch id derivation",
                                ))?;
                                let reservation_id = ids.harness_mcp_reservation_id
                                    .ok_or(HarnessRuntimeError::DispatchPreparation(
                                        "dispatch ids carry no harness mcp reservation id",
                                    ))?;
                                let (grant_id, grant_revision) = resolve_harness_mcp_grant(
                                    &mut harness,
                                    &intent.operation_id,
                                    &plan.grant,
                                    &run.run_id,
                                    HarnessGrantTargetV1 {
                                        node_id: context.node_id.clone(),
                                        workspace_id: context.workspace_id.clone(),
                                        provider_profile: context.provider_profile.clone(),
                                        mode: context.mode,
                                    },
                                    now,
                                )?;
                                let expires_at_unix_ms = now.checked_add(
                                    plan.deadline_ms.min(
                                        gate4agent_node_protocol::MAX_HARNESS_MCP_RESERVATION_TTL_MS,
                                    ),
                                ).ok_or(HarnessRuntimeError::DispatchPreparation(
                                    "harness mcp reservation expiry overflowed u64",
                                ))?;
                                let prepared = harness.begin_run_dispatch_with_harness_mcp(
                                    run.revision,
                                    dispatching_run,
                                    operation.revision,
                                    dispatching_operation,
                                    context,
                                    &spec,
                                    reservation_id,
                                    grant_id,
                                    grant_revision,
                                    expires_at_unix_ms,
                                )?;
                                let pending = adapter.start_arm_harness_mcp_reservation(
                                    &harness,
                                    &route,
                                    prepared,
                                    &spec,
                                )?;
                                start_harness_mcp_arm_finish(
                                    commands.clone(),
                                    intent.operation_id.clone(),
                                    spec,
                                    profile,
                                    pending,
                                );
                                Ok(CoordinatorPreflightStart::HarnessMcpArm { plan: plan.clone() })
                            } else {
                                let prepared = harness.issue_spawn_lease(
                                    &effective_launch,
                                    run.revision,
                                    dispatching_run,
                                    operation.revision,
                                    dispatching_operation,
                                    context,
                                    spec,
                                )?;
                                let pending = match prepared {
                                    PreparedScheduledSpawnLease::Direct(prepared) => {
                                        PendingCoordinatorSpawn::Direct(
                                            adapter.start_prepared_spawn(prepared, profile)?,
                                        )
                                    }
                                    PreparedScheduledSpawnLease::Managed(prepared) => {
                                        PendingCoordinatorSpawn::Managed(
                                            adapter.start_prepared_managed_worktree_spawn(
                                                prepared,
                                                profile,
                                            )?,
                                        )
                                    }
                                };
                                Ok(CoordinatorPreflightStart::Spawn { route, pending, plan: plan.clone() })
                            }
                            })();
                            match preparation {
                                Ok(CoordinatorPreflightStart::Spawn { route, pending, plan }) => {
                                    start_dispatch_finish(
                                        adapter.clone(),
                                        commands.clone(),
                                        intent.operation_id.clone(),
                                        route,
                                        pending,
                                    );
                                    active_dispatch = Some(ActiveDispatchJob::new(
                                        intent.operation_id,
                                        CoordinatorDispatchPhase::Spawn,
                                        plan,
                                    ));
                                }
                                Ok(CoordinatorPreflightStart::HarnessMcpArm { plan }) => {
                                    active_dispatch = Some(ActiveDispatchJob::new(
                                        intent.operation_id,
                                        CoordinatorDispatchPhase::HarnessMcpArm,
                                        plan,
                                    ));
                                }
                                Err(error) => {
                                    active_dispatch = None;
                                    if harness.engine().operation(&intent.operation_id)
                                        .is_some_and(|operation| {
                                            operation.state
                                                == HarnessOperationStateV1::Dispatching
                                        })
                                    {
                                        if let Some(reservation_id) = apply_spawn_result(
                                            &mut harness,
                                            &effective_launch_catalog(
                                                &catalogs.launch,
                                                &runtime_inventory,
                                            ).0,
                                            &intent.operation_id,
                                            dispatching_start_error_result(&error),
                                            unix_time_ms(),
                                        )? {
                                            if let Some(cleanup) = harness
                                                .harness_mcp_reservation(&reservation_id)
                                                .and_then(pending_harness_mcp_abort)
                                            {
                                                pending_harness_mcp_aborts
                                                    .entry(reservation_id)
                                                    .or_insert(cleanup);
                                            }
                                        }
                                    } else {
                                        // The sibling arm above routes its
                                        // error through a classifier that
                                        // names it; this one discarded it
                                        // outright, which is how a run could
                                        // end terminal-Failed with its cause
                                        // recorded nowhere at all.
                                        note_terminal_pre_dispatch("dispatch-job", &error);
                                        if let Some(reservation_id) = apply_pre_dispatch_result(
                                            &mut harness,
                                            &intent.operation_id,
                                            CoordinatorPreDispatchResult::Failed,
                                            unix_time_ms(),
                                        )? {
                                            if let Some(cleanup) = harness
                                                .harness_mcp_reservation(&reservation_id)
                                                .and_then(pending_harness_mcp_abort)
                                            {
                                                pending_harness_mcp_aborts
                                                    .entry(reservation_id)
                                                    .or_insert(cleanup);
                                            }
                                        }
                                    }
                                }
                            }
                            notify_task_changed(&mut subscribers, &harness, &notify_task_id);
                            notify_run_changed(&mut subscribers, &harness, &notify_run_id);
                        }
                        Some(HostCommand::HarnessMcpActivationFinished {
                            reservation_id,
                            attempt_id,
                            expected_revision,
                            result,
                        }) => {
                            if !harness_mcp_workers.accepts_activation(
                                &reservation_id,
                                attempt_id,
                                expected_revision,
                            ) {
                                continue;
                            }
                            let active = harness_mcp_workers.activations
                                .remove(&reservation_id)
                                .expect("accepted activation remains present");
                            let current_is_exact = harness
                                .harness_mcp_reservation(&reservation_id)
                                .is_some_and(|reservation| {
                                    reservation.revision == expected_revision
                                });
                            let outcome = if !current_is_exact {
                                Err(HarnessRuntimeError::HarnessMcpAuthority)
                            } else {
                                match result {
                                    Ok(proof) if proof.reservation_id() == &reservation_id => {
                                        harness.record_harness_mcp_active(
                                            proof,
                                            unix_time_ms().max(active.updated_at_unix_ms),
                                        ).map_err(HarnessRuntimeError::Harness)
                                    }
                                    Ok(_) => Err(HarnessRuntimeError::HarnessMcpAuthority),
                                    Err(error) => Err(HarnessRuntimeError::C2(error)),
                                }
                            };
                            if let Some(reply) = active.reply {
                                let _ = reply.send(outcome);
                            }
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::HarnessMcpAbortFinished {
                            reservation_id,
                            attempt_id,
                            result,
                        }) => {
                            let accepts = pending_harness_mcp_aborts
                                .get(&reservation_id)
                                .is_some_and(|pending| pending.accepts_completion(attempt_id));
                            if !accepts { continue; }
                            if result.is_ok() || matches!(
                                result,
                                Err(HarnessC2Error::HarnessMcpRejected {
                                    code: NodeFailureCode::ReservationNotFound,
                                })
                            ) {
                                pending_harness_mcp_aborts.remove(&reservation_id);
                            } else if let Some(cleanup) = pending_harness_mcp_aborts
                                .get_mut(&reservation_id)
                            {
                                cleanup.attempt_id = None;
                                defer_harness_mcp_abort(cleanup, unix_time_ms());
                            }
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::HarnessMcpRelayFinished {
                            reservation_id,
                            call_id,
                            attempt_id,
                            result,
                        }) => {
                            if !harness_mcp_workers.accepts_relay(
                                &reservation_id,
                                &call_id,
                                attempt_id,
                            ) {
                                continue;
                            }
                            harness_mcp_workers.relays.remove(&(
                                reservation_id,
                                call_id,
                            ));
                            let _ = result;
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::DispatchFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Spawn)
                            }) {
                                continue;
                            }
                            // The plan resolves against a single-entry catalog
                            // built from what this job already resolved when it
                            // started -- not a fresh `effective_launch_catalog`
                            // call -- because the Control event that reports
                            // this very spawn as accepted is also one of the
                            // events that invalidates the target node's cached
                            // runtime inventory (see `ActiveDispatchJob::plan`).
                            // Re-deriving here raced that invalidation and lost.
                            let resolved_plan = active_dispatch.take()
                                .map(|job| job.plan)
                                .ok_or(HarnessRuntimeError::DispatchPreparation(
                                    "active dispatch job is missing for the finished spawn",
                                ))?;
                            let resolved_launch = HarnessLaunchCatalog::new([resolved_plan])
                                .unwrap_or_default();
                            if let Some(reservation_id) = apply_spawn_result(
                                &mut harness,
                                &resolved_launch,
                                &operation_id,
                                result,
                                unix_time_ms(),
                            )? {
                                if let Some(cleanup) = harness
                                    .harness_mcp_reservation(&reservation_id)
                                    .and_then(pending_harness_mcp_abort)
                                {
                                    pending_harness_mcp_aborts
                                        .entry(reservation_id)
                                        .or_insert(cleanup);
                                }
                            }
                            notify_operation_touched(&mut subscribers, &harness, &operation_id);
                        }
                        Some(HostCommand::HarnessMcpArmFinished {
                            operation_id,
                            spec,
                            profile,
                            result,
                        }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::HarnessMcpArm)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            // `operation_id` moves into `active_dispatch` on
                            // the success path below, so this notification
                            // id is captured up front rather than reused
                            // after the move.
                            let notify_operation_id = operation_id.clone();
                            // Shared by every branch below (`record_harness_
                            // mcp_armed_and_issue_spawn_lease` and both
                            // `apply_spawn_result` fallbacks) so they all
                            // resolve against the exact same launch catalog
                            // snapshot -- see `effective_launch_catalog`.
                            let (effective_launch, _truncated) =
                                effective_launch_catalog(&catalogs.launch, &runtime_inventory);
                            match result {
                                Ok(proof) => {
                                    let pending = (|| -> Result<_, HarnessRuntimeError> {
                                        let prepared = harness
                                            .record_harness_mcp_armed_and_issue_spawn_lease(
                                            &effective_launch,
                                            proof,
                                            unix_time_ms(),
                                            spec,
                                        )?;
                                        Ok(PendingCoordinatorSpawn::Direct(
                                            adapter.start_prepared_spawn(prepared, profile)?,
                                        ))
                                    })();
                                    match pending {
                                        Ok(pending) => {
                                            let route = pending_harness_mcp_spawn_route(
                                                &harness,
                                                &operation_id,
                                            )?;
                                            start_dispatch_finish(
                                                adapter.clone(),
                                                commands.clone(),
                                                operation_id.clone(),
                                                route,
                                                pending,
                                            );
                                            // Grant-bound (harness-MCP) plans are
                                            // always CLI-authored -- see
                                            // `derived_launch_plan` -- so
                                            // `effective_launch` resolving this
                                            // scheduled ref is exactly as stable
                                            // here as it always was.
                                            let armed_plan = harness.scheduled_launch(&operation_id)
                                                .and_then(|scheduled| {
                                                    effective_launch.resolve_scheduled(scheduled).ok()
                                                })
                                                .ok_or(HarnessRuntimeError::DispatchPreparation(
                                                    "operation has no scheduled launch resolving against the launch catalog",
                                                ))?
                                                .clone();
                                            active_dispatch = Some(ActiveDispatchJob::new(
                                                operation_id,
                                                CoordinatorDispatchPhase::Spawn,
                                                armed_plan,
                                            ));
                                        }
                                        Err(error) => {
                                            if let Some(reservation_id) = apply_spawn_result(
                                                &mut harness,
                                                &effective_launch,
                                                &operation_id,
                                                dispatching_start_error_result(&error),
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    if let Some(reservation_id) = apply_spawn_result(
                                        &mut harness,
                                        &effective_launch,
                                        &operation_id,
                                        harness_mcp_arm_finish_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                }
                            }
                            notify_operation_touched(&mut subscribers, &harness, &notify_operation_id);
                        }
                        Some(HostCommand::DeliveryStageFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Delivery)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            match result {
                                Ok(proof) => {
                                    let stage_result = (|| {
                                        let delivery = harness.engine().deliveries()
                                            .find(|delivery| {
                                                delivery.operation_id == operation_id
                                            })
                                            .ok_or(HarnessRuntimeError::DispatchPreparation(
                                                "no delivery record matches the finished stage operation",
                                            ))?
                                            .clone();
                                        harness.stage_delivery_with_proof(
                                            delivery.revision,
                                            &delivery.delivery_ref,
                                            unix_time_ms(),
                                            &adapter,
                                            proof,
                                        ).map_err(HarnessRuntimeError::Harness)
                                    })();
                                    if let Err(error) = stage_result {
                                        if let Some(reservation_id) = apply_pre_dispatch_result(
                                            &mut harness,
                                            &operation_id,
                                            delivery_stage_completion_result(&error),
                                            unix_time_ms(),
                                        )? {
                                            if let Some(cleanup) = harness
                                                .harness_mcp_reservation(&reservation_id)
                                                .and_then(pending_harness_mcp_abort)
                                            {
                                                pending_harness_mcp_aborts
                                                    .entry(reservation_id)
                                                    .or_insert(cleanup);
                                            }
                                        }
                                        notify_operation_touched(&mut subscribers, &harness, &operation_id);
                                        continue;
                                    }
                                    let intent = harness.pending_scheduled_dispatch()?
                                        .filter(|intent| intent.operation_id == operation_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                                            "no pending scheduled dispatch for this operation",
                                        ))?;
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &runtime_inventory,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                                Err(error) => {
                                    if let Some(reservation_id) = apply_pre_dispatch_result(
                                        &mut harness,
                                        &operation_id,
                                        delivery_pre_dispatch_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                }
                            }
                            notify_operation_touched(&mut subscribers, &harness, &operation_id);
                        }
                        Some(HostCommand::ContinuationExportFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Continuation)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            match result {
                                Ok(outcome) => {
                                    harness.apply_continuation_export_outcome(
                                        outcome,
                                        unix_time_ms(),
                                    )?;
                                    let run_id = harness.engine().operation(&operation_id)
                                        .and_then(|operation| operation.run_id.as_ref())
                                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                                            "operation carries no run id",
                                        ))?
                                        .clone();
                                    let continuation = harness.engine()
                                        .continuation_for_run(&run_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                                            "run has no continuation record",
                                        ))?
                                        .clone();
                                    match continuation.state {
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::Exported => {
                                            let intent = harness.pending_scheduled_dispatch()?
                                                .filter(|intent| {
                                                    intent.operation_id == operation_id
                                                })
                                                .ok_or(HarnessRuntimeError::DispatchPreparation(
                                                    "no pending scheduled dispatch for this operation",
                                                ))?;
                                            active_dispatch = start_or_terminalize_dispatch_job(
                                                &mut harness,
                                                &adapter,
                                                &commands,
                                                &catalogs,
                                                &runtime_inventory,
                                                &mut pending_harness_mcp_aborts,
                                                intent,
                                            )?;
                                        }
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::OutcomeUnknown => {
                                            if let Some(reservation_id) = apply_pre_dispatch_result(
                                                &mut harness,
                                                &operation_id,
                                                CoordinatorPreDispatchResult::OutcomeUnknown,
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::Expired => {
                                            if let Some(reservation_id) = apply_pre_dispatch_result(
                                                &mut harness,
                                                &operation_id,
                                                CoordinatorPreDispatchResult::Failed,
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                        _ => return Err(HarnessRuntimeError::DispatchPreparation(
                                            "continuation is neither Exported, OutcomeUnknown nor Expired after export finished",
                                        )),
                                    }
                                }
                                Err(_) => {
                                    let run_id = harness.engine().operation(&operation_id)
                                        .and_then(|operation| operation.run_id.as_ref())
                                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                                            "operation carries no run id",
                                        ))?
                                        .clone();
                                    let continuation = harness.engine()
                                        .continuation_for_run(&run_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation(
                                            "run has no continuation record",
                                        ))?
                                        .clone();
                                    if continuation.state
                                        == gate4agent_harness_protocol::HarnessContinuationStateV1::Exporting
                                    {
                                        harness.recover_exporting_continuation_outcome_unknown(
                                            &continuation.continuation_ref,
                                            continuation.revision,
                                            unix_time_ms(),
                                        )?;
                                    }
                                    apply_pre_dispatch_result(
                                        &mut harness,
                                        &operation_id,
                                        CoordinatorPreDispatchResult::OutcomeUnknown,
                                        unix_time_ms(),
                                    )?;
                                }
                            }
                            notify_operation_touched(&mut subscribers, &harness, &operation_id);
                        }
                        Some(HostCommand::ObservationRecoveryFinished {
                            route,
                            attempt_id,
                            requested_after,
                            result,
                        }) => {
                            finish_observation_recovery(
                                &mut observation_recovery,
                                &mut harness,
                                &mut observation,
                                &mut support,
                                &mut runtime_inventory,
                                &mut subscribers,
                                route,
                                attempt_id,
                                requested_after,
                                result,
                            )?;
                        }
                        Some(HostCommand::RunReadFinished { completion, reply }) => {
                            run_read_workers.finish();
                            let (prepared, result) = completion.into_parts();
                            let reply_value = match validate_run_read_completion_origin(
                                harness.engine().run(prepared.run_id()),
                                &prepared,
                            ) {
                                Err(error) => HarnessOperatorReplyV1::Error { error },
                                Ok(()) => match result {
                                    Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                    Err(error) => HarnessOperatorReplyV1::Error {
                                        error: map_run_read_error(error),
                                    },
                                },
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::NodeWorkspaceReadFinished { result, reply, identity }) => {
                            node_workspace_read_workers.finish();
                            let reply_value = match result {
                                Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        cause = %cause,
                                        "node workspace read rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_node_workspace_read_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::NodeWorkspaceWriteFinished { result, reply, identity }) => {
                            node_workspace_write_workers.finish();
                            let reply_value = match result {
                                Ok(response) => {
                                    // Unlike a read, a settled write/create is
                                    // a state change on the node's disk --
                                    // logged at INFO, the same way a settled
                                    // `SessionSpawnFinished::Accepted` is.
                                    tracing::info!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        path = identity.path(),
                                        "node workspace write applied by the node",
                                    );
                                    HarnessOperatorReplyV1::Ok { response }
                                }
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        path = identity.path(),
                                        cause = %cause,
                                        "node workspace write rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_node_workspace_write_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::SessionSpawnFinished {
                            route, result, reply, identity, requested_transport,
                        }) => {
                            session_spawn_workers.finish();
                            let reply_value = match result {
                                Ok(SpawnDispatchOutcome::Accepted(receipt)) => {
                                    tracing::info!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        provider = identity.provider(),
                                        provider_profile = identity.provider_profile(),
                                        session = ?receipt.session(),
                                        "session spawn accepted by the node",
                                    );
                                    HarnessOperatorReplyV1::Ok {
                                        response: HarnessOperatorResponseV1::SessionSpawned(
                                            session_address_from_receipt(&route, receipt.session()),
                                        ),
                                    }
                                }
                                Ok(SpawnDispatchOutcome::Rejected { code }) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        provider = identity.provider(),
                                        provider_profile = identity.provider_profile(),
                                        cause = ?code,
                                        "session spawn rejected by the node",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_session_spawn_node_failure(
                                            code,
                                            identity.provider(),
                                            requested_transport,
                                        ),
                                    }
                                }
                                Ok(SpawnDispatchOutcome::OutcomeUnknown { reason }) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        provider = identity.provider(),
                                        provider_profile = identity.provider_profile(),
                                        reason = ?reason,
                                        "session spawn outcome unknown after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::OutcomeUnknown,
                                    }
                                }
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        provider = identity.provider(),
                                        provider_profile = identity.provider_profile(),
                                        cause = %cause,
                                        "session spawn C2 dispatch failed",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_session_spawn_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::SessionControlFinished {
                            result,
                            reply,
                            identity,
                            route,
                            roster_effect,
                        }) => {
                            session_control_workers.finish();
                            let reply_value = match result {
                                Ok(()) => HarnessOperatorReplyV1::Ok {
                                    response: session_control_response(&identity),
                                },
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        session_id = identity.session_id(),
                                        cause = %cause,
                                        "session control request rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_session_control_error(cause),
                                    }
                                }
                            };
                            // `route`/`roster_effect` were captured from
                            // `PendingSessionControl` before its own C2
                            // round trip even started (see
                            // `PendingSessionControl::roster_effect`'s doc
                            // comment), so -- unlike `identity`'s log-only
                            // strings -- both are already-resolved,
                            // structured values; nothing left to re-parse or
                            // re-look-up here.
                            match roster_effect {
                                SessionRosterEffect::None => {}
                                SessionRosterEffect::Absent(session) => {
                                    tracing::info!(
                                        node_id = identity.node_id(),
                                        session_id = identity.session_id(),
                                        "stop/remove-session accepted; invalidating the node's runtime inventory route",
                                    );
                                    // Same call set as the reactive live-event
                                    // path (which does NOT mark the route
                                    // unhealthy): the recovery sweep picks the
                                    // ensured route up on the next loop pass.
                                    // The session address is recorded as a
                                    // pending expectation on that same route
                                    // entry so a resync that lands before the
                                    // node's own internal session-list update
                                    // catches up gets retried instead of
                                    // accepted as final -- see
                                    // `RouteObservationRecovery::
                                    // awaiting_absent_sessions`.
                                    invalidate_runtime_inventory_for_route(
                                        &mut observation_recovery,
                                        &route,
                                    );
                                    observation_recovery.ensure_route(route)
                                        .awaiting_absent_sessions.push(session);
                                }
                                SessionRosterEffect::Changed => {
                                    tracing::info!(
                                        node_id = identity.node_id(),
                                        session_id = identity.session_id(),
                                        "resume-session accepted; invalidating the node's runtime inventory route",
                                    );
                                    // No `awaiting_absent_sessions` entry:
                                    // unlike `Absent`, a resumed session is
                                    // expected to reappear, not disappear --
                                    // an ordinary targeted resync is enough
                                    // (see `SessionRosterEffect::Changed`'s
                                    // doc comment).
                                    invalidate_runtime_inventory_for_route(
                                        &mut observation_recovery,
                                        &route,
                                    );
                                }
                            }
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::SessionRecordMutationFinished {
                            result,
                            reply,
                            identity,
                            route,
                        }) => {
                            session_record_mutation_workers.finish();
                            let reply_value = match result {
                                Ok(response) => {
                                    // Unconditional, unlike `SessionControlFinished`'s
                                    // per-verb `SessionRosterEffect` -- see
                                    // `HostCommand::SessionRecordMutationFinished`'s
                                    // doc comment.
                                    tracing::info!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        "session record mutation accepted; invalidating the node's runtime inventory route",
                                    );
                                    invalidate_runtime_inventory_for_route(
                                        &mut observation_recovery,
                                        &route,
                                    );
                                    HarnessOperatorReplyV1::Ok { response }
                                }
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        session_id = identity.session_id(),
                                        cause = %cause,
                                        "session record mutation request rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_session_record_mutation_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::HostDirectoryBrowseFinished { result, reply, identity }) => {
                            host_directory_browse_workers.finish();
                            let reply_value = match result {
                                Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        path = identity.path(),
                                        cause = %cause,
                                        "host directory browse request rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_host_directory_browse_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::ResourceMutationFinished {
                            result,
                            reply,
                            identity,
                            route,
                            invalidates_runtime_inventory,
                        }) => {
                            resource_mutation_workers.finish();
                            let reply_value = match result {
                                Ok(response) => {
                                    if invalidates_runtime_inventory {
                                        tracing::info!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            workspace_id = identity.workspace_id(),
                                            "resource mutation accepted; invalidating the node's runtime inventory route",
                                        );
                                        invalidate_runtime_inventory_for_route(
                                            &mut observation_recovery,
                                            &route,
                                        );
                                    } else {
                                        tracing::info!(
                                            operation = %identity.operation,
                                            node_id = identity.node_id(),
                                            "resource mutation accepted",
                                        );
                                    }
                                    HarnessOperatorReplyV1::Ok { response }
                                }
                                Err(cause) => {
                                    tracing::warn!(
                                        operation = %identity.operation,
                                        node_id = identity.node_id(),
                                        workspace_id = identity.workspace_id(),
                                        source_workspace_id = identity.source_workspace_id(),
                                        path = identity.path(),
                                        cause = %cause,
                                        "resource mutation request rejected after its C2 round trip",
                                    );
                                    HarnessOperatorReplyV1::Error {
                                        error: map_resource_mutation_error(cause),
                                    }
                                }
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::RunGitFactsCaptureFinished { run_id, completion }) => {
                            run_git_facts_workers.finish();
                            let notify_run_id = run_id.clone();
                            finish_run_git_facts_capture(
                                &mut harness,
                                run_id,
                                completion,
                                unix_time_ms(),
                            );
                            notify_run_changed(&mut subscribers, &harness, &notify_run_id);
                        }
                        Some(HostCommand::RunContextSourceFinished { completion, reply }) => {
                            run_context_source_workers.finish();
                            let (prepared, result) = completion.into_parts();
                            let current = harness.engine().run(prepared.run_id());
                            let origin = match current {
                                None => Err(HarnessOperatorHostErrorV1::NotFound),
                                Some(run) if run.binding.as_ref() != Some(prepared.binding()) => {
                                    Err(HarnessOperatorHostErrorV1::Conflict)
                                }
                                Some(run) => Ok(run),
                            };
                            match (origin, result) {
                                (Err(error), _) => {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                }
                                (_, Err(error)) => {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: map_run_context_source_error(error),
                                    });
                                }
                                (Ok(run), Ok(RunContextSourceProjection::SupportedNotObserved)) => {
                                    let observation = unobserved_run_context_source(
                                        run,
                                        FeatureObservationStateV1::SupportedNotObserved,
                                    );
                                    let _ = reply.send(HarnessOperatorReplyV1::Ok {
                                        response: HarnessOperatorResponseV1::RunContextSourceObserved(
                                            observation,
                                        ),
                                    });
                                }
                                (Ok(_), Ok(projection)) => {
                                    let recovery_route = prepared.route().clone();
                                    let recovery_run_id = prepared.run_id().clone();
                                    let recovery_after = prepared.observed_after_sequence();
                                    let deadline = Instant::from_std(
                                        prepared.started_at()
                                            + RUN_CONTEXT_SOURCE_TOTAL_BUDGET,
                                    );
                                    pending_run_context_sources.push(PendingRunContextSourceReply {
                                        prepared,
                                        projection,
                                        deadline,
                                        reply,
                                    });
                                    poll_pending_run_context_sources(
                                        &harness,
                                        &mut observation,
                                        &support,
                                        &runtime_inventory,
                                        &mut pending_run_context_sources,
                                    );
                                    ensure_run_context_source_recovery_if_pending(
                                        &pending_run_context_sources,
                                        &mut observation_recovery,
                                        &recovery_run_id,
                                        &recovery_route,
                                        recovery_after,
                                    );
                                }
                            }
                        }
                        Some(HostCommand::NativeHistoryWorkerFinished) => {
                            native_history_workers.finish();
                        }
                        Some(HostCommand::Subscribe { sender, identity }) => {
                            tracing::info!(
                                operation = %identity.operation,
                                node_id = identity.node_id(),
                                "harness operator event subscriber registered",
                            );
                            let id = subscribers.insert(sender, identity);
                            let (tasks, runs) = harness_snapshot_baseline_payload(&harness);
                            let nodes = runtime_inventory.all_nodes();
                            subscribers.send_to(id, |sequence| {
                                HarnessOperatorEventV1::SnapshotBaseline { sequence, tasks, runs, nodes }
                            });
                        }
                        Some(HostCommand::SubscribeTerminal { sender, sessions, identity }) => {
                            tracing::info!(
                                operation = %identity.operation,
                                "harness terminal event subscriber registered",
                            );
                            let id = terminal_subscribers.insert(
                                sender,
                                sessions.clone(),
                                identity,
                            );
                            for key in &sessions {
                                if let Some(frame) = terminal_buffers.latest(key) {
                                    terminal_subscribers.send_to(id, key, frame.clone());
                                }
                            }
                        }
                        Some(HostCommand::SubscribeAgentStream { sender, sessions, identity }) => {
                            tracing::info!(
                                operation = %identity.operation,
                                sessions = ?sessions,
                                "harness agent stream subscriber registered",
                            );
                            // Unlike `SubscribeTerminal` immediately above,
                            // there is no explicit seed call here to make --
                            // `AgentStreamSubscriberRegistry::insert` itself
                            // seeds the new subscriber with whatever state it
                            // already holds for `sessions` (see its own doc
                            // comment in `agent_stream.rs`).
                            agent_stream_subscribers.insert(sender, sessions, identity);
                        }
                        Some(HostCommand::Shutdown { reply }) => {
                            let result = harness.flush().map_err(HarnessRuntimeError::Harness)
                                .and_then(|_| observation.flush().map_err(HarnessRuntimeError::Observation));
                            let successful = result.is_ok();
                            let _ = reply.send(result);
                            if successful { return Ok(()); }
                            return Err(HarnessRuntimeError::FlushFailed);
                        }
                        None => return Err(HarnessRuntimeError::HostStopped),
                    }
                }
                event = events.recv(), if events_open => {
                    match event {
                        Some(event) => {
                            match &event.event {
                                C2NodeEvent::TerminalFrame { address, frame } => {
                                    let key = RuntimeSessionKey {
                                        node_id: event.node_id.clone(),
                                        incarnation_id: event.cursor.incarnation_id,
                                        workspace_id: address.workspace_id.clone(),
                                        instance_id: address.session.instance_id,
                                        generation: address.session.generation,
                                    };
                                    terminal_buffers.ingest(key.clone(), frame.clone());
                                    terminal_subscribers.publish(&key, frame);
                                    // The frame is also how the inventory
                                    // learns this session's screen changed
                                    // -- see `apply_screen_state` for why
                                    // the resync cadence is not enough for
                                    // this one field.
                                    if let Some(node) = runtime_inventory
                                        .apply_screen_state(&key, &frame.screen_state)
                                    {
                                        let node = node.clone();
                                        subscribers.emit(|sequence| {
                                            HarnessOperatorEventV1::RuntimeInventoryChanged {
                                                sequence,
                                                node: node.clone(),
                                            }
                                        });
                                    }
                                }
                                C2NodeEvent::AgentStream { address, chunk } => {
                                    let key = RuntimeSessionKey {
                                        node_id: event.node_id.clone(),
                                        incarnation_id: event.cursor.incarnation_id,
                                        workspace_id: address.workspace_id.clone(),
                                        instance_id: address.session.instance_id,
                                        generation: address.session.generation,
                                    };
                                    agent_stream_subscribers.publish(&key, chunk);
                                }
                                // The generic sink for every way an
                                // interaction can settle -- an operator's own
                                // `ResolveInteraction` (via its C2 round
                                // trip and the node's own report back) or
                                // `HostPolicy` deciding it on a deadline both
                                // land here the same way, since both produce
                                // the same observation kind. See
                                // `AgentStreamSubscriberRegistry::
                                // resolve_interaction`'s own doc comment
                                // (`agent_stream.rs`) for why a resolved
                                // prompt must leave the agent-stream seed set
                                // regardless of which of the two caused it.
                                C2NodeEvent::Observation { address, observation } => {
                                    if let gate4agent_observation_protocol::ObservationKindV1::ApprovalResolved {
                                        correlation_id, ..
                                    }
                                    | gate4agent_observation_protocol::ObservationKindV1::QuestionResolved {
                                        correlation_id, ..
                                    }
                                    | gate4agent_observation_protocol::ObservationKindV1::InteractionResolved {
                                        correlation_id, ..
                                    } = &observation.kind
                                    {
                                        let key = RuntimeSessionKey {
                                            node_id: event.node_id.clone(),
                                            incarnation_id: event.cursor.incarnation_id,
                                            workspace_id: address.workspace_id.clone(),
                                            instance_id: address.session.instance_id,
                                            generation: address.session.generation,
                                        };
                                        agent_stream_subscribers.resolve_interaction(&key, correlation_id);
                                    }
                                }
                                C2NodeEvent::ResyncRequired { .. } => {
                                    terminal_buffers.invalidate(&NodeRoute {
                                        node_id: event.node_id.clone(),
                                        expected_incarnation_id: event.cursor.incarnation_id,
                                    });
                                }
                                _ => {}
                            }
                            let result = if matches!(
                                event.event,
                                C2NodeEvent::HarnessMcpReadCall { .. }
                            ) {
                                let plan = prepare_harness_mcp_read_call(
                                    &adapter,
                                    &harness,
                                    &observation,
                                    &support,
                                    event,
                                )?;
                                let _ = schedule_harness_mcp_relay(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &pending_harness_mcp_aborts,
                                    &harness_mcp_rejects,
                                    plan,
                                )?;
                                Ok(())
                            } else {
                                apply_or_buffer_host_live_event(
                                    &adapter,
                                    &mut harness,
                                    &mut observation,
                                    &mut support,
                                    &mut observation_recovery,
                                    &mut subscribers,
                                    event,
                                )
                            };
                            if let Err(error) = result {
                                if !matches!(error, HarnessRuntimeError::C2(_)) {
                                    return Err(error);
                                }
                            }
                        }
                        None => {
                            support.mark_all_unhealthy();
                            events_open = false;
                        }
                    }
                }
                changed = topology.changed(), if topology_open => {
                    match changed {
                        Ok(routes) => {
                            support.mark_all_unhealthy();
                            let routes = routes.into_iter()
                                .map(|observation_route| observation_route.route().clone())
                                .collect::<Vec<_>>();
                            observation_recovery.reconcile_topology(&routes);
                            // Read back off the same watch the `changed()`
                            // above just woke on -- `routes` is only the
                            // online subset, and departure cannot be read
                            // from that subset alone. See
                            // `HarnessC2TopologyReceiver::known_node_ids`.
                            let known_node_ids = topology.known_node_ids();
                            for removed_node_id in
                                runtime_inventory.reconcile_topology(&routes, &known_node_ids)
                            {
                                subscribers.emit(|sequence| {
                                    HarnessOperatorEventV1::RuntimeInventoryRemoved {
                                        sequence,
                                        node_id: removed_node_id.as_str().to_owned(),
                                    }
                                });
                            }
                            terminal_buffers.reconcile_topology(&routes);
                            for route in &routes {
                                support.mark_unhealthy(
                                    &route.node_id,
                                    route.expected_incarnation_id,
                                );
                            }
                            if active_dispatch.is_none() {
                                if let Some(intent) = harness.pending_scheduled_dispatch()? {
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &runtime_inventory,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                            }
                        }
                        Err(_) => {
                            support.mark_all_unhealthy();
                            topology_open = false;
                        }
                    }
                }
                _ = run_context_source_poll.tick(), if !pending_run_context_sources.is_empty() => {
                    poll_pending_run_context_sources(
                        &harness,
                        &mut observation,
                        &support,
                        &runtime_inventory,
                        &mut pending_run_context_sources,
                    );
                }
                _ = recovery_retry.tick() => {
                    let routes = adapter.observation_routes();
                    support.reconcile_current_routes(&routes);
                    for route in routes {
                        if support.is_authoritative(
                            &route.node_id,
                            route.expected_incarnation_id,
                        ) {
                            continue;
                        }
                        support.mark_unhealthy(
                            &route.node_id,
                            route.expected_incarnation_id,
                        );
                        observation_recovery.ensure_route(route);
                    }
                    let harness_mcp_actions = prepare_harness_mcp_reconcile(
                        &mut harness,
                        &adapter,
                        &observation,
                        &support,
                    )?;
                    schedule_harness_mcp_reconcile_workers(
                        &adapter,
                        &commands,
                        &mut harness_mcp_workers,
                        harness_mcp_actions,
                        &mut pending_harness_mcp_aborts,
                    );
                    start_pending_harness_mcp_abort_workers(
                        &adapter,
                        &commands,
                        &mut harness_mcp_workers,
                        &mut pending_harness_mcp_aborts,
                    );
                    if active_dispatch.is_none() {
                        if let Some(intent) = harness.pending_scheduled_dispatch()? {
                            active_dispatch = start_or_terminalize_dispatch_job(
                                &mut harness,
                                &adapter,
                                &commands,
                                &catalogs,
                                &runtime_inventory,
                                &mut pending_harness_mcp_aborts,
                                intent,
                            )?;
                        }
                    }
                }
                _ = run_git_facts_sweep.tick(), if run_git_facts_workers.has_capacity() => {
                    reconcile_run_git_facts_capture(
                        &harness,
                        &adapter,
                        &mut run_git_facts_workers,
                        &commands,
                    );
                }
                _ = subscriber_keepalive.tick(), if !subscribers.is_empty()
                    || !terminal_subscribers.is_empty()
                    || !agent_stream_subscribers.is_empty() => {
                    emit_subscriber_keepalive(&mut subscribers);
                    terminal_subscribers.keepalive();
                    agent_stream_subscribers.keepalive();
                }
                accepted = listener.accept() => {
                    let (stream, peer) = accepted.map_err(|_| HarnessRuntimeError::AcceptFailed)?;
                    if !peer.ip().is_loopback() { continue; }
                    let Ok(permit) = connections.clone().try_acquire_owned() else { continue; };
                    let request_commands = commands.clone();
                    let request_operator_authority = operator_authority.clone();
                    let request_subscriber_connections = subscriber_connections.clone();
                    let request_terminal_subscriber_connections =
                        terminal_subscriber_connections.clone();
                    let request_agent_stream_subscriber_connections =
                        agent_stream_subscriber_connections.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(
                            stream,
                            request_commands,
                            request_operator_authority,
                            permit,
                            request_subscriber_connections,
                            request_terminal_subscriber_connections,
                            request_agent_stream_subscriber_connections,
                        ).await;
                    });
                }
            }
            subscribers.recover_lagged(&harness, &runtime_inventory);
            terminal_subscribers.flush_pending();
            agent_stream_subscribers.flush_lagged();
            start_pending_observation_recoveries(
                &adapter,
                &commands,
                &observation,
                &mut observation_recovery,
            );
        }
        }.await;
        if let Err(ref error) = result {
            // The one log line every silent-exit path above now shares:
            // whichever `return Err(_)` ended the loop, this fires before
            // the task (and with it `listener`, `command_rx`, and every
            // subscriber registry) is dropped -- naming the failure instead
            // of leaving the operator wire's port to vanish unexplained
            // while the rest of the process stays up.
            tracing::error!(
                error = %error,
                "harness runtime loop exiting; operator wire is stopping while the process stays up",
            );
        }
        result
    });
    Ok((handle, task))
}

fn scheduled_dispatch_from_operator_response(
    response: &HarnessOperatorResponseV1,
) -> Option<HarnessDispatchIntentV1> {
    match response {
        HarnessOperatorResponseV1::Schedule(
            gate4agent_harness_protocol::HarnessScheduleOutcomeV1::Dispatch(intent),
        ) => Some(intent.clone()),
        HarnessOperatorResponseV1::TaskStarted(outcome) if !outcome.replayed => {
            Some(outcome.dispatch.clone())
        }
        _ => None,
    }
}

async fn execute_harness_mcp_reconcile(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    actions: Vec<HarnessMcpReconcileAction>,
    pending_aborts: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) -> Result<(), HarnessRuntimeError> {
    for action in actions {
        match action {
            HarnessMcpReconcileAction::Activate {
                route,
                reservation,
                record_id,
                session,
                updated_at_unix_ms,
            } => {
                if let Ok(proof) = adapter.activate_harness_mcp_reservation(
                    &route,
                    reservation,
                    record_id,
                    session,
                ).await {
                    harness.record_harness_mcp_active(
                        proof,
                        unix_time_ms().max(updated_at_unix_ms),
                    )?;
                }
            }
            HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            } => {
                pending_aborts.entry(reservation_id.clone()).or_insert(
                    PendingHarnessMcpAbort {
                        route,
                        reservation_id,
                        activation_digest,
                        attempts: 0,
                        retry_after_unix_ms: 0,
                        attempt_id: None,
                    },
                );
            }
        }
    }
    Ok(())
}

fn schedule_harness_mcp_reconcile_workers(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    actions: Vec<HarnessMcpReconcileAction>,
    pending_aborts: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    for action in actions {
        match action {
            HarnessMcpReconcileAction::Activate {
                route,
                reservation,
                record_id,
                session,
                updated_at_unix_ms,
            } => {
                let _ = schedule_harness_mcp_activation(
                    adapter,
                    commands,
                    workers,
                    pending_aborts,
                    route,
                    reservation,
                    record_id,
                    session,
                    updated_at_unix_ms,
                    None,
                );
            }
            HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            } => {
                pending_aborts.entry(reservation_id.clone()).or_insert(
                    PendingHarnessMcpAbort {
                        route,
                        reservation_id,
                        activation_digest,
                        attempts: 0,
                        retry_after_unix_ms: 0,
                        attempt_id: None,
                    },
                );
            }
        }
    }
}

#[derive(Clone)]
struct PendingHarnessMcpAbort {
    route: NodeRoute,
    reservation_id: HarnessMcpReservationId,
    activation_digest: HarnessMcpActivationDigest,
    attempts: u8,
    retry_after_unix_ms: u64,
    attempt_id: Option<u64>,
}

impl PendingHarnessMcpAbort {
    fn accepts_completion(&self, attempt_id: u64) -> bool {
        self.attempt_id == Some(attempt_id)
    }
}

fn schedule_harness_mcp_activation(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    record_id: SessionRecordId,
    session: SessionAddress,
    updated_at_unix_ms: u64,
    reply: Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>,
) -> Result<(), Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>> {
    let reservation_id = reservation.reservation_id.clone();
    if workers.activations.contains_key(&reservation_id)
        || !workers.has_capacity(pending_aborts)
    {
        return Err(reply);
    }
    let attempt_id = workers.allocate_attempt_id();
    let expected_revision = reservation.revision;
    workers.activations.insert(
        reservation_id,
        ActiveHarnessMcpActivation {
            attempt_id,
            expected_revision,
            updated_at_unix_ms,
            reply,
        },
    );
    start_harness_mcp_activation_finish(
        adapter.clone(),
        commands.clone(),
        route,
        reservation,
        record_id,
        session,
        attempt_id,
        expected_revision,
    );
    Ok(())
}

fn start_pending_harness_mcp_abort_workers(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    let current_routes = adapter.observation_routes();
    let now = unix_time_ms();
    let reservation_ids = pending.keys().cloned().collect::<Vec<_>>();
    for reservation_id in reservation_ids {
        if !workers.has_capacity(pending) { break; }
        let Some(cleanup) = pending.get(&reservation_id) else { continue; };
        if cleanup.attempt_id.is_some() || now < cleanup.retry_after_unix_ms {
            continue;
        }
        let Some(current_route) = current_routes.iter()
            .find(|route| route.node_id == cleanup.route.node_id) else {
                continue;
            };
        if current_route != &cleanup.route {
            pending.remove(&reservation_id);
            continue;
        }
        let attempt_id = workers.allocate_attempt_id();
        let cleanup = pending.get_mut(&reservation_id)
            .expect("selected pending abort remains present");
        cleanup.attempt_id = Some(attempt_id);
        start_harness_mcp_abort_finish(
            adapter.clone(),
            commands.clone(),
            cleanup.clone(),
            attempt_id,
        );
    }
}

fn schedule_harness_mcp_relay(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    capacity_rejects: &mpsc::Sender<HarnessMcpRelayPlan>,
    plan: HarnessMcpRelayPlan,
) -> Result<bool, HarnessRuntimeError> {
    let key = (plan.reservation_id.clone(), plan.call_id.clone());
    if workers.relays.contains_key(&key) {
        return Ok(false);
    }
    if !workers.has_capacity(pending_aborts) {
        enqueue_harness_mcp_capacity_rejection(capacity_rejects, plan)?;
        return Ok(true);
    }
    let attempt_id = workers.allocate_attempt_id();
    workers.relays.insert(key, ActiveHarnessMcpRelay { attempt_id });
    start_harness_mcp_relay_finish(
        adapter.clone(),
        commands.clone(),
        plan,
        attempt_id,
    );
    Ok(true)
}

fn enqueue_harness_mcp_capacity_rejection(
    capacity_rejects: &mpsc::Sender<HarnessMcpRelayPlan>,
    mut plan: HarnessMcpRelayPlan,
) -> Result<(), HarnessRuntimeError> {
    plan.outcome = Err(HarnessMcpRejectReasonV1::Internal);
    capacity_rejects.try_send(plan)
        .map_err(|_| HarnessRuntimeError::HarnessMcpRejectQueueFull)
}

fn durable_harness_mcp_abort_cleanup(
    harness: &HarnessService,
) -> BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort> {
    harness.harness_mcp_reservations.values()
        .filter(|reservation| {
            reservation.state == crate::HarnessMcpReservationStateV1::Revoked
        })
        .filter_map(|reservation| pending_harness_mcp_abort(reservation))
        .map(|pending| (pending.reservation_id.clone(), pending))
        .collect()
}

fn non_revoked_harness_mcp_abort_cleanup(
    harness: &HarnessService,
) -> BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort> {
    harness.harness_mcp_reservations.values()
        .filter(|reservation| {
            reservation.state != crate::HarnessMcpReservationStateV1::Revoked
        })
        .filter_map(|reservation| pending_harness_mcp_abort(reservation))
        .map(|pending| (pending.reservation_id.clone(), pending))
        .collect()
}

fn pending_harness_mcp_abort(
    reservation: &crate::HarnessMcpReservationV1,
) -> Option<PendingHarnessMcpAbort> {
    Some(PendingHarnessMcpAbort {
        route: reservation_route(reservation).ok()?,
        reservation_id: reservation.reservation_id.clone(),
        activation_digest: reservation.activation_digest.clone(),
        attempts: 0,
        retry_after_unix_ms: 0,
        attempt_id: None,
    })
}

fn enqueue_newly_revoked_harness_mcp_aborts(
    harness: &HarnessService,
    prior: BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    for (reservation_id, cleanup) in prior {
        if harness.harness_mcp_reservation_state(&reservation_id)
            == Some(crate::HarnessMcpReservationStateV1::Revoked)
        {
            pending.entry(reservation_id).or_insert(cleanup);
        }
    }
}

async fn retry_pending_harness_mcp_aborts(
    adapter: &HarnessC2Adapter,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    let current_routes = adapter.observation_routes();
    let now = unix_time_ms();
    let reservation_ids = pending.keys().cloned().collect::<Vec<_>>();
    for reservation_id in reservation_ids {
        let Some(cleanup) = pending.get(&reservation_id).cloned() else { continue; };
        if now < cleanup.retry_after_unix_ms {
            continue;
        }
        let Some(current_route) = current_routes.iter()
            .find(|route| route.node_id == cleanup.route.node_id) else {
                continue;
            };
        if current_route != &cleanup.route {
            pending.remove(&reservation_id);
            continue;
        }
        let result = adapter.abort_harness_mcp_reservation(
            &cleanup.route,
            &cleanup.reservation_id,
            &cleanup.activation_digest,
        ).await;
        if result.is_ok() || matches!(
            result,
            Err(HarnessC2Error::HarnessMcpRejected {
                code: NodeFailureCode::ReservationNotFound,
            })
        ) {
            pending.remove(&reservation_id);
            continue;
        }
        if let Some(cleanup) = pending.get_mut(&reservation_id) {
            defer_harness_mcp_abort(cleanup, now);
        }
    }
}

fn defer_harness_mcp_abort(cleanup: &mut PendingHarnessMcpAbort, now_unix_ms: u64) {
    cleanup.attempts = cleanup.attempts.saturating_add(1);
    let shift = u32::from(cleanup.attempts.min(5));
    let retry_ms = 1_000_u64.checked_shl(shift)
        .unwrap_or(HARNESS_MCP_ABORT_RETRY_MAX_MS)
        .min(HARNESS_MCP_ABORT_RETRY_MAX_MS);
    cleanup.retry_after_unix_ms = now_unix_ms.saturating_add(retry_ms);
}

enum HarnessMcpReconcileAction {
    Activate {
        route: NodeRoute,
        reservation: crate::HarnessMcpReservationV1,
        record_id: SessionRecordId,
        session: SessionAddress,
        updated_at_unix_ms: u64,
    },
    Abort {
        route: NodeRoute,
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
    },
}

fn prepare_harness_mcp_reconcile(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
) -> Result<Vec<HarnessMcpReconcileAction>, HarnessRuntimeError> {
    let now = unix_time_ms();
    let current_routes = adapter.observation_routes();
    let mut actions = Vec::new();
    let candidates = harness.harness_mcp_reservations.values()
        .map(|reservation| (
            reservation.reservation_id.clone(),
            reservation.state,
            reservation.activation_digest.clone(),
            reservation.expires_at_unix_ms,
            reservation.updated_at_unix_ms,
        ))
        .collect::<Vec<_>>();
    for (
        reservation_id,
        state,
        activation_digest,
        expires_at_unix_ms,
        updated_at_unix_ms,
    ) in candidates {
        let route = harness.harness_mcp_reservation(&reservation_id)
            .and_then(|reservation| reservation_route(reservation).ok());
        let bootstrap_expired = matches!(
            state,
            crate::HarnessMcpReservationStateV1::Prepared
                | crate::HarnessMcpReservationStateV1::Armed
                | crate::HarnessMcpReservationStateV1::Bound
        ) && now >= expires_at_unix_ms;
        if bootstrap_expired {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            if let Some(route) = route {
                actions.push(HarnessMcpReconcileAction::Abort {
                    route,
                    reservation_id,
                    activation_digest,
                });
            }
            continue;
        }
        if !matches!(
            state,
            crate::HarnessMcpReservationStateV1::Bound
                | crate::HarnessMcpReservationStateV1::Active
        ) {
            continue;
        }
        let route = match route {
            Some(route) => route,
            None => {
                harness.revoke_harness_mcp_reservation(
                    &reservation_id,
                    now.max(updated_at_unix_ms),
                )?;
                continue;
            }
        };
        let Some(current_route) = current_routes.iter()
            .find(|current| current.node_id == route.node_id) else {
                continue;
            };
        if current_route != &route {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            actions.push(HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            });
            continue;
        }
        let binding = match harness.harness_mcp_binding_for_reconcile(&reservation_id) {
            Ok(binding) => binding,
            Err(_) => {
                harness.revoke_harness_mcp_reservation(
                    &reservation_id,
                    now.max(updated_at_unix_ms),
                )?;
                actions.push(HarnessMcpReconcileAction::Abort {
                    route,
                    reservation_id,
                    activation_digest,
                });
                continue;
            }
        };
        if !support.is_authoritative(&route.node_id, route.expected_incarnation_id) {
            continue;
        }
        if verify_observation_credential_binding(observation, support, &binding).is_err() {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            actions.push(HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            });
            continue;
        }
        let Ok((reservation, record_id, session)) = harness
            .validate_activatable_harness_mcp_authority(&reservation_id, now) else {
            continue;
        };
        actions.push(HarnessMcpReconcileAction::Activate {
            route,
            reservation,
            record_id,
            session,
            updated_at_unix_ms,
        });
    }
    Ok(actions)
}

fn reservation_route(
    reservation: &crate::HarnessMcpReservationV1,
) -> Result<NodeRoute, HarnessRuntimeError> {
    Ok(NodeRoute {
        node_id: NodeId::new(reservation.node_id.as_str())
            .map_err(|_| HarnessRuntimeError::HarnessMcpAuthority)?,
        expected_incarnation_id: reservation.node_incarnation_id.as_str().parse()
            .map_err(|_| HarnessRuntimeError::HarnessMcpAuthority)?,
    })
}

fn pending_harness_mcp_spawn_route(
    harness: &HarnessService,
    operation_id: &HarnessOperationId,
) -> Result<NodeRoute, HarnessRuntimeError> {
    let reservation = harness.harness_mcp_reservations.values()
        .find(|reservation| &reservation.operation_id == operation_id)
        .ok_or(HarnessRuntimeError::HarnessMcpAuthority)?;
    reservation_route(reservation)
}

#[derive(Clone)]
struct HarnessOperatorCredentialAuthority {
    digest: [u8; 32],
}

impl HarnessOperatorCredentialAuthority {
    fn new(credential: HarnessOperatorCredential) -> Result<Self, HarnessRuntimeError> {
        let digest = operator_credential_digest(&credential)?;
        drop(credential);
        Ok(Self { digest })
    }

    fn verify(&self, credential: &HarnessOperatorCredential) -> Result<bool, HarnessRuntimeError> {
        let actual = operator_credential_digest(credential)?;
        Ok(proofs_match(&actual, &self.digest))
    }
}

#[derive(Default)]
pub(crate) struct HarnessRuntimeInventoryCache {
    nodes: BTreeMap<NodeId, HarnessRuntimeNodeInventoryV1>,
    managed_worktree_profiles: BTreeMap<NodeId, ManagedWorktreeProfileOptionsCache>,
}

#[derive(Default)]
struct ManagedWorktreeProfileOptionsCache {
    node_incarnation: String,
    profiles: Vec<HarnessManagedWorktreeProfileOptionV1>,
    truncated: bool,
}

impl HarnessRuntimeInventoryCache {
    /// Applies a terminal frame's screen classification onto the cached
    /// session, returning the refreshed node projection when the value
    /// actually changed.
    ///
    /// `refresh` is the only other writer here and it runs exclusively on
    /// an observation resync -- a recovery path. That is the right cadence
    /// for everything else the projection carries, all of which changes
    /// only when a session's lifecycle does. `screen_state` is the one
    /// field that does not: a pane can go from the agent's own composer to
    /// a vendor-update prompt without any lifecycle event at all, and an
    /// inventory that only learns about it at the next resync would keep
    /// answering `Ready` for a screen that has since stopped being ready.
    /// For a value whose entire purpose is gating work, stale-optimistic is
    /// the one direction that must not happen.
    ///
    /// This needs no new transport: `C2NodeEvent::TerminalFrame` already
    /// arrives continuously to fill the terminal ring, and it already
    /// carries the classification stamped at that frame. The node only
    /// republishes a changed classification, but a frame carries the
    /// current one on every frame, so the comparison below is what keeps
    /// this from emitting an inventory event per frame.
    fn apply_screen_state(
        &mut self,
        key: &RuntimeSessionKey,
        screen_state: &gate4agent_types::PtyScreenState,
    ) -> Option<&HarnessRuntimeNodeInventoryV1> {
        let node = self.nodes.get_mut(&key.node_id)?;
        if node.incarnation_id != key.incarnation_id.to_string() {
            return None;
        }
        let workspace = node.inventory.workspaces.get_mut(key.workspace_id.as_str())?;
        let session = workspace.sessions.iter_mut().find(|session| {
            session.instance_id == key.instance_id.0 && session.generation == key.generation.0
        })?;
        let projected = Some(crate::terminal::map_screen_state(screen_state));
        if session.screen_state == projected {
            return None;
        }
        session.screen_state = projected;
        self.nodes.get(&key.node_id)
    }

    /// Best-effort lookup of a live session's cached transport and provider
    /// id, for `prompt_session_pty_refusal`'s dispatch-time PTY check --
    /// see that function's doc comment for why a miss here is read as "not
    /// provably PTY" rather than a defect. Transport is fixed for a
    /// session's entire lifetime once spawned (a resume never changes it),
    /// so unlike `screen_state` a HIT here is never stale in a way that
    /// matters, even under this cache's normal resync lag. Same lookup
    /// shape as `apply_screen_state` immediately above, read-only.
    fn session_transport(
        &self,
        key: &RuntimeSessionKey,
    ) -> Option<(HarnessRuntimeTransportV1, String)> {
        let node = self.nodes.get(&key.node_id)?;
        if node.incarnation_id != key.incarnation_id.to_string() {
            return None;
        }
        let workspace = node.inventory.workspaces.get(key.workspace_id.as_str())?;
        let session = workspace.sessions.iter().find(|session| {
            session.instance_id == key.instance_id.0 && session.generation == key.generation.0
        })?;
        Some((session.transport, session.provider.clone()))
    }

    /// Returns the freshly built node projection when it actually differs
    /// from whatever was cached for this node id before this call (or when
    /// there was nothing cached yet) -- `None` when the refresh landed the
    /// exact same value again (`HarnessRuntimeNodeInventoryV1` is `Eq`), so
    /// a caller pushing `RuntimeInventoryChanged` events can skip emitting a
    /// no-op change.
    fn refresh(
        &mut self,
        resync: &HarnessObservationResync,
        observed_at_unix_ms: u64,
    ) -> Option<HarnessRuntimeNodeInventoryV1> {
        let route = resync.route();
        let mut profiles = Vec::new();
        for workspace in &resync.snapshot().workspaces {
            let Some(inventory) = &workspace.managed_worktree_profiles else { continue; };
            for profile in &inventory.profiles {
                let projected = (|| {
                    Some(HarnessManagedWorktreeProfileOptionV1 {
                        node_id: HarnessSelectorV1::new(route.node_id.as_str()).ok()?,
                        node_incarnation: HarnessSelectorV1::new(
                            route.expected_incarnation_id.to_string(),
                        ).ok()?,
                        source_workspace_id: HarnessSelectorV1::new(
                            workspace.workspace_id.as_str(),
                        ).ok()?,
                        profile_id: HarnessSelectorV1::new(profile.id.as_str()).ok()?,
                        profile_revision: HarnessSelectorV1::new(
                            profile.revision.as_str(),
                        ).ok()?,
                        retention: match profile.retention {
                            gate4agent_node_protocol::ManagedWorktreeRetention::RemoveWhenReleased => {
                                HarnessManagedWorktreeRetentionV1::RemoveWhenReleased
                            }
                            gate4agent_node_protocol::ManagedWorktreeRetention::Retain => {
                                HarnessManagedWorktreeRetentionV1::Retain
                            }
                        },
                        observed_at_unix_ms,
                    })
                })();
                if let Some(projected) = projected { profiles.push(projected); }
            }
        }
        profiles.sort_by(|left, right| {
            (
                &left.node_id,
                &left.source_workspace_id,
                &left.profile_id,
                &left.profile_revision,
            ).cmp(&(
                &right.node_id,
                &right.source_workspace_id,
                &right.profile_id,
                &right.profile_revision,
            ))
        });
        profiles.dedup_by(|left, right| {
            left.node_id == right.node_id
                && left.source_workspace_id == right.source_workspace_id
                && left.profile_id == right.profile_id
                && left.profile_revision == right.profile_revision
        });
        let truncated = profiles.len() > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX;
        profiles.truncate(gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX);
        self.managed_worktree_profiles.insert(
            route.node_id.clone(),
            ManagedWorktreeProfileOptionsCache {
                node_incarnation: route.expected_incarnation_id.to_string(),
                profiles,
                truncated,
            },
        );
        let node = HarnessRuntimeNodeInventoryV1 {
            node_id: route.node_id.as_str().to_owned(),
            incarnation_id: route.expected_incarnation_id.to_string(),
            observed_at_unix_ms,
            event_sequence: resync.event_sequence(),
            inventory: redact_runtime_inventory(
                gate4agent_c2_protocol::SlimNodeInventory::from_c2_snapshot(resync.snapshot()),
            ),
        };
        let changed = self.nodes.get(&route.node_id) != Some(&node);
        self.nodes.insert(route.node_id.clone(), node.clone());
        changed.then_some(node)
    }

    /// Returns the ids of every node this topology change proves has
    /// DEPARTED, so a caller can emit `RuntimeInventoryRemoved` per departed
    /// node rather than guessing from the `retain` predicate. This is the
    /// only place in the harness that observes a node departing, and so the
    /// only source a `RuntimeInventoryRemoved` may come from.
    ///
    /// `routes` carries only the nodes that are `Online` this instant;
    /// `known_node_ids` carries every node the C2 knows about at all. The
    /// difference is the whole point. A node missing from `routes` but still
    /// in `known_node_ids` has NOT departed -- its relay is reconnecting,
    /// which happens on any request that overruns its bound, on backoff, on
    /// a pipe hiccup, and lasts a blink while the node process and every PTY
    /// under it keep running. Reading absence from `routes` alone published
    /// a removal for that blink, and a removal tears the operator's live
    /// tabs down. Such a node is kept, deliberately stale, until the resync
    /// that follows its return replaces it.
    ///
    /// Departure is therefore exactly two things: the id is gone from the
    /// C2's roster entirely, or the id is back online under a DIFFERENT
    /// incarnation, which means the node process restarted and nothing
    /// cached about the old one survives.
    fn reconcile_topology(
        &mut self,
        routes: &[NodeRoute],
        known_node_ids: &BTreeSet<NodeId>,
    ) -> Vec<NodeId> {
        let departed = |node_id: &NodeId, incarnation_id: &str| -> bool {
            if !known_node_ids.contains(node_id) {
                return true;
            }
            routes.iter().any(|route| {
                &route.node_id == node_id
                    && route.expected_incarnation_id.to_string() != incarnation_id
            })
        };
        let before = self.nodes.keys().cloned().collect::<Vec<_>>();
        self.nodes.retain(|node_id, inventory| !departed(node_id, &inventory.incarnation_id));
        self.managed_worktree_profiles
            .retain(|node_id, profiles| !departed(node_id, &profiles.node_incarnation));
        before.into_iter().filter(|node_id| !self.nodes.contains_key(node_id)).collect()
    }

    /// Every currently cached node, unpaginated -- the runtime-inventory
    /// counterpart of `harness_snapshot_baseline_payload`'s task/run lists,
    /// used to build a `SnapshotBaseline` (which is a full replacement, not
    /// a page, so it deliberately does not go through `page`'s
    /// `HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX` cap).
    fn all_nodes(&self) -> Vec<HarnessRuntimeNodeInventoryV1> {
        self.nodes.values().cloned().collect()
    }

    /// The single cached node by id, regardless of whether the most recent
    /// `refresh` actually changed it (`refresh` returns `None` on an
    /// unchanged value; this still finds it) -- used by
    /// `finish_observation_recovery` to check `awaiting_absent_sessions`
    /// against whatever is now cached.
    fn node(&self, node_id: &NodeId) -> Option<&HarnessRuntimeNodeInventoryV1> {
        self.nodes.get(node_id)
    }

    fn page(
        &self,
        after_node_id: Option<&str>,
        limit: u16,
    ) -> HarnessRuntimeInventoryPageV1 {
        let mut nodes = self.nodes.values()
            .filter(|node| match after_node_id {
                Some(after) => node.node_id.as_str() > after,
                None => true,
            })
            .take(usize::from(limit) + 1)
            .cloned()
            .collect::<Vec<_>>();
        let has_more = nodes.len() > usize::from(limit);
        if has_more { nodes.pop(); }
        let next_cursor = has_more.then(|| {
            nodes.last().expect("nonzero runtime inventory page limit").node_id.clone()
        });
        HarnessRuntimeInventoryPageV1 { nodes, next_cursor }
    }

    fn correlation_availability(
        &self,
        run: &gate4agent_harness_protocol::HarnessRunV1,
        binding: &HarnessSessionBindingV1,
    ) -> (HarnessRunCorrelationAvailabilityV1, Option<u64>) {
        let Some(node) = self.nodes.values().find(|node| {
            node.node_id == binding.node_id.as_str()
        }) else {
            return (HarnessRunCorrelationAvailabilityV1::NotObserved, None);
        };
        let observed_at = Some(node.observed_at_unix_ms);
        if node.incarnation_id != binding.node_incarnation.as_str() {
            return (
                HarnessRunCorrelationAvailabilityV1::StaleIncarnation,
                observed_at,
            );
        }
        let HarnessSessionIdentityV1::Managed {
            record_id,
            active_session,
        } = &binding.session else {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        };
        let Some(record) = node.inventory.managed_sessions.iter().find(|record| {
            record.record_id == record_id.as_str()
        }) else {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        };
        let expected_mode = match run.intent.mode {
            HarnessExecutionModeV1::Pty => HarnessRuntimeManagedModeV1::Pty,
            HarnessExecutionModeV1::Inline => HarnessRuntimeManagedModeV1::Inline,
            HarnessExecutionModeV1::Acp => HarnessRuntimeManagedModeV1::Acp,
        };
        if record.workspace_id != binding.workspace_id.as_str()
            || record.mode != expected_mode
        {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        }
        let exact_active = active_session.as_ref().is_some_and(|active| {
            record.active_binding.as_ref().is_some_and(|current| {
                current.workspace_id == binding.workspace_id.as_str()
                    && current.instance_id == active.instance_id
                    && current.generation == active.generation
            })
        });
        if matches!(
            record.state,
            HarnessRuntimeManagedStateV1::Live
                | HarnessRuntimeManagedStateV1::IdentityPending
        ) && exact_active
        {
            return (HarnessRunCorrelationAvailabilityV1::Available, observed_at);
        }
        if active_session.is_none()
            && record.state == HarnessRuntimeManagedStateV1::Dormant
            && record.active_binding.is_none()
        {
            return (HarnessRunCorrelationAvailabilityV1::Dormant, observed_at);
        }
        (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at)
    }
}

/// Composes the harness's effective launch catalog for one call: every
/// explicitly configured (CLI `--launch-plan-json`) plan first, then one
/// synthesized ordinary plan per node/workspace/provider/spawn-profile
/// combination `runtime_inventory` currently advertises
/// (`derive_launch_plans_from_inventory`) for every combination whose id
/// does not collide with a CLI plan id -- the CLI plan always wins on
/// collision, and a derived plan that cannot fit within
/// `HARNESS_LAUNCH_CATALOG_MAX` alongside the CLI plans is dropped (the
/// returned `bool` reports whether that happened).
///
/// Recomputed fresh on every call -- nothing here is cached beyond what
/// `runtime_inventory` itself already is, so a node that joins the fleet
/// after startup contributes plans on the very next call and one that
/// leaves stops contributing just as immediately, no restart required.
/// Building `HarnessLaunchCatalog::new` from this composition can fail
/// only if a derived plan id collided with another derived plan id, which
/// `derive_launch_plans_from_inventory` already prevents by construction
/// (see its own doc comment) -- the CLI-only fallback below exists purely
/// so a defect in that invariant degrades to the explicit configuration
/// instead of panicking or losing it too.
fn effective_launch_catalog(
    cli: &HarnessLaunchCatalog,
    runtime_inventory: &HarnessRuntimeInventoryCache,
) -> (HarnessLaunchCatalog, bool) {
    let mut plans = cli.all_plans().cloned().collect::<Vec<_>>();
    let capacity = HARNESS_LAUNCH_CATALOG_MAX.saturating_sub(plans.len());
    let mut truncated = false;
    let mut added = 0usize;
    for plan in derive_launch_plans_from_inventory(&runtime_inventory.all_nodes()) {
        if cli.contains(&plan.plan_id) {
            continue;
        }
        if added >= capacity {
            truncated = true;
            continue;
        }
        plans.push(plan);
        added += 1;
    }
    let catalog = HarnessLaunchCatalog::new(plans).unwrap_or_else(|_| cli.clone());
    (catalog, truncated)
}

/// Logs once, at INFO, when the launch plan durably scheduled for
/// `operation_id` is not one of the explicitly configured CLI plans in
/// `cli_launch_catalog` -- i.e. the harness just dispatched a task using a
/// plan it synthesized from the live runtime inventory
/// (`derive_launch_plans_from_inventory`) rather than an operator-authored
/// `--launch-plan-json` entry. Called once per freshly created schedule
/// (`ScheduleNext`, `StartTask`, and `StartTaskV2` all funnel through the
/// same `scheduled_dispatch_from_operator_response` check at the call
/// site), not on every resume of an already-scheduled dispatch, so this
/// stays a per-dispatch signal instead of firing on every inventory
/// refresh or replay.
fn log_if_derived_launch_plan_used(
    harness: &HarnessService,
    cli_launch_catalog: &HarnessLaunchCatalog,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    operation_id: &HarnessOperationId,
) {
    let Some(scheduled) = harness.scheduled_launch(operation_id) else { return; };
    if cli_launch_catalog.contains(&scheduled.plan.plan_id) {
        return;
    }
    let (effective_launch, _truncated) =
        effective_launch_catalog(cli_launch_catalog, runtime_inventory);
    let Ok(plan) = effective_launch.resolve_scheduled(scheduled) else { return; };
    tracing::info!(
        plan_id = scheduled.plan.plan_id.as_str(),
        node_id = plan.node_id.as_str(),
        provider = plan.provider.as_str(),
        "derived launch plan used for a new task dispatch",
    );
}

fn project_operator_run_correlation(
    harness: &HarnessService,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
) -> Result<HarnessRunCorrelationV1, HarnessOperatorHostErrorV1> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    project_run_correlation(run, runtime_inventory)
}

fn project_operator_run_transfer(
    harness: &HarnessService,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
) -> Result<HarnessRunTransferSummaryV1, HarnessOperatorHostErrorV1> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    project_run_transfer(
        run,
        harness.engine().delivery_for_run(run_id),
        harness.engine().continuation_for_run(run_id),
    )
}

fn project_run_transfer(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    delivery: Option<&gate4agent_harness_protocol::HarnessDeliveryV1>,
    continuation: Option<&gate4agent_harness_protocol::HarnessContinuationV1>,
) -> Result<HarnessRunTransferSummaryV1, HarnessOperatorHostErrorV1> {
    run.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let delivery = delivery.map(|delivery| {
        delivery.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        if delivery.run_id != run.run_id || delivery.task_id != run.task_id {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        Ok(HarnessRunDeliveryTransferV1 {
            delivery_ref: delivery.delivery_ref.clone(),
            revision: delivery.revision,
            state: delivery.state,
            selector: delivery.bundle.selector.clone(),
            bundle_id: delivery.bundle.bundle_id.clone(),
            bundle_revision: delivery.bundle.revision.clone(),
            bundle_digest: delivery.bundle.digest.clone(),
            manifest_digest: delivery.bundle.manifest_digest.clone(),
            receipt_ref: delivery.receipt.as_ref().map(|receipt| receipt.receipt_ref.clone()),
            created_at_unix_ms: delivery.created_at_unix_ms,
            updated_at_unix_ms: delivery.updated_at_unix_ms,
            staged_at_unix_ms: delivery.stage_receipt.as_ref()
                .map(|receipt| receipt.staged_at_unix_ms),
            committed_at_unix_ms: delivery.receipt.as_ref()
                .map(|receipt| receipt.committed_at_unix_ms),
        })
    }).transpose()?;
    let continuation = continuation.map(|continuation| {
        continuation.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        if continuation.target_run_id != run.run_id {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        let context = continuation.context.as_ref().map(|context| {
            HarnessRunContextTransferV1 {
                context_ref: context.id.clone(),
                digest: context.digest.clone(),
                source_message_count: context.source_message_count,
                retained_message_count: context.retained_message_count,
                byte_len: context.byte_len,
                truncated: context.truncated,
            }
        });
        Ok(HarnessRunContinuationTransferV1 {
            continuation_ref: continuation.continuation_ref.clone(),
            receipt_ref: continuation.receipt_ref.clone(),
            revision: continuation.revision,
            state: continuation.state,
            source_run_id: continuation.source_run_id.clone(),
            target_run_id: continuation.target_run_id.clone(),
            source_provider: continuation.source_provider.clone(),
            context,
            prepared_at_unix_ms: continuation.prepared_at_unix_ms,
            exporting_at_unix_ms: continuation.exporting_at_unix_ms,
            exported_at_unix_ms: continuation.exported_at_unix_ms,
            bound_at_unix_ms: continuation.bound_at_unix_ms,
            expired_at_unix_ms: continuation.expired_at_unix_ms,
            outcome_unknown_at_unix_ms: continuation.outcome_unknown_at_unix_ms,
            outcome_unknown_reason: continuation.outcome_unknown_reason,
            created_at_unix_ms: continuation.created_at_unix_ms,
            updated_at_unix_ms: continuation.updated_at_unix_ms,
        })
    }).transpose()?;
    let transfer = HarnessRunTransferSummaryV1 {
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        delivery,
        continuation,
    };
    transfer.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(transfer)
}

fn project_run_correlation(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    runtime_inventory: &HarnessRuntimeInventoryCache,
) -> Result<HarnessRunCorrelationV1, HarnessOperatorHostErrorV1> {
    run.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let binding = run.binding.as_ref()
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    let node_incarnation_id = HarnessNodeIncarnationV1::new(
        binding.node_incarnation.as_str(),
    ).map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let worktree = match &run.intent.worktree {
        HarnessWorktreeIntentV1::Existing => HarnessRunWorktreeViewV1::Existing,
        HarnessWorktreeIntentV1::Managed { worktree_ref } => {
            HarnessRunWorktreeViewV1::Managed {
                worktree_ref: worktree_ref.clone(),
            }
        }
        HarnessWorktreeIntentV1::ManagedProfile { .. } => {
            HarnessRunWorktreeViewV1::Managed {
                worktree_ref: binding.workspace_id.clone(),
            }
        }
    };
    let session = match &binding.session {
        HarnessSessionIdentityV1::Managed { record_id, active_session } => {
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: record_id.clone(),
                active_session: active_session.clone(),
            })
        }
        HarnessSessionIdentityV1::Inline { inline_ref } => {
            HarnessRunSessionViewV1::Inline(HarnessInlineRunSessionV1 {
                inline_ref: inline_ref.clone(),
            })
        }
    };
    let (availability, observed_at_unix_ms) =
        runtime_inventory.correlation_availability(run, binding);
    let correlation = HarnessRunCorrelationV1 {
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        task_id: run.task_id.clone(),
        node_id: binding.node_id.clone(),
        node_incarnation_id,
        workspace_id: binding.workspace_id.clone(),
        provider_profile: run.intent.provider_profile.clone(),
        mode: run.intent.mode,
        worktree,
        session,
        availability,
        observed_at_unix_ms,
    };
    correlation.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    Ok(correlation)
}

/// Projects a `SlimNodeInventory` (itself already the C2-side projection of
/// one node's `C2NodeSnapshot`, via `SlimNodeInventory::from_c2_snapshot`)
/// into the operator wire's `HarnessRuntimeInventoryV1`: workspace/session/
/// managed-session/launch-inventory field mapping, with no other input and
/// no side effects.
///
/// Promoted `pub` for `gate4agent-harness-light`: this is exactly the
/// projection that crate's own runtime-inventory maintenance needs
/// (`RuntimeInventoryList`'s served-from-cache path), and it is pure data
/// mapping with zero kernel entanglement -- unlike
/// `HarnessRuntimeInventoryCache` itself (`pub(crate)`, keyed to this
/// crate's own `HarnessObservationResync`/observation-recovery machinery,
/// which the light harness has no equivalent for and does not want; see
/// `gate4agent-harness-light::inventory`'s module doc for why that cache is
/// reimplemented light-local instead of reused). Reusing this one function
/// keeps both harnesses' `HarnessRuntimeInventoryV1` projection identical by
/// construction, with no duplicated field-mapping logic to drift.
///
/// Always populates `screen_state: Some(..)` from the source `SlimSession`,
/// unconditionally -- this wire has exactly one accepted version (see
/// `HARNESS_OPERATOR_WIRE_VERSION`), so every peer sees the same
/// projection, cached once and fanned out to every currently
/// polling/subscribed peer with no per-recipient variation.
pub fn redact_runtime_inventory(
    inventory: gate4agent_c2_protocol::SlimNodeInventory,
) -> HarnessRuntimeInventoryV1 {
    let enabled_providers = inventory.enabled_providers.into_iter()
        .map(|provider| provider.as_str().to_owned())
        .collect();
    let workspaces = inventory.workspaces.into_iter().map(|(_, workspace)| {
        let workspace_id = workspace.workspace_id.as_str().to_owned();
        let sessions = workspace.sessions.into_iter().map(|session| HarnessRuntimeSessionV1 {
            instance_id: session.instance_id.0,
            generation: session.generation.0,
            provider: session.agent_id,
            transport: match session.transport {
                gate4agent_types::TransportKind::Pty => HarnessRuntimeTransportV1::Pty,
                gate4agent_types::TransportKind::Pipe => HarnessRuntimeTransportV1::Pipe,
                gate4agent_types::TransportKind::Acp => HarnessRuntimeTransportV1::Acp,
            },
            status: match session.status {
                gate4agent_c2_protocol::SlimSessionStatus::Registered => {
                    HarnessRuntimeSessionStatusV1::Registered
                }
                gate4agent_c2_protocol::SlimSessionStatus::Starting => {
                    HarnessRuntimeSessionStatusV1::Starting
                }
                gate4agent_c2_protocol::SlimSessionStatus::Running => {
                    HarnessRuntimeSessionStatusV1::Running
                }
                gate4agent_c2_protocol::SlimSessionStatus::Stopping => {
                    HarnessRuntimeSessionStatusV1::Stopping
                }
                gate4agent_c2_protocol::SlimSessionStatus::Exited => {
                    HarnessRuntimeSessionStatusV1::Exited
                }
                gate4agent_c2_protocol::SlimSessionStatus::Failed => {
                    HarnessRuntimeSessionStatusV1::Failed
                }
            },
            process_id: session.process_id,
            terminal_size: session.terminal_size.map(|size| HarnessRuntimeTerminalSizeV1 {
                rows: size.rows,
                columns: size.columns,
            }),
            operation_pending: session.operation_pending,
            input_pending: session.input_pending,
            // Unconditional `Some` -- see this function's own doc comment
            // for why the version gate does not belong here.
            screen_state: Some(map_screen_state(&session.screen_state)),
        }).collect();
        let redacted = HarnessRuntimeWorkspaceV1 {
            workspace_id: workspace_id.clone(),
            display_root: workspace.canonical_root,
            display_root_truncated: workspace.canonical_root_truncated,
            sessions,
            session_count: workspace.session_count,
            sessions_truncated: workspace.sessions_truncated,
        };
        (workspace_id, redacted)
    }).collect();
    let managed_sessions = inventory.managed_sessions.into_iter().map(|record| {
        HarnessRuntimeManagedSessionV1 {
            record_id: record.record_id.as_str().to_owned(),
            display_name: record.display_name,
            display_name_truncated: record.display_name_truncated,
            provider: record.provider.as_str().to_owned(),
            mode: match record.mode {
                gate4agent_node_protocol::SessionMode::Pty => HarnessRuntimeManagedModeV1::Pty,
                gate4agent_node_protocol::SessionMode::Inline => {
                    HarnessRuntimeManagedModeV1::Inline
                }
                gate4agent_node_protocol::SessionMode::Acp => HarnessRuntimeManagedModeV1::Acp,
            },
            state: match record.state {
                gate4agent_node_protocol::ManagedSessionState::IdentityPending => {
                    HarnessRuntimeManagedStateV1::IdentityPending
                }
                gate4agent_node_protocol::ManagedSessionState::Live => {
                    HarnessRuntimeManagedStateV1::Live
                }
                gate4agent_node_protocol::ManagedSessionState::Dormant => {
                    HarnessRuntimeManagedStateV1::Dormant
                }
                gate4agent_node_protocol::ManagedSessionState::Unavailable => {
                    HarnessRuntimeManagedStateV1::Unavailable
                }
            },
            workspace_id: record.workspace_id.as_str().to_owned(),
            active_binding: record.active_session.map(|address| HarnessRuntimeSessionBindingV1 {
                workspace_id: address.workspace_id.as_str().to_owned(),
                instance_id: address.session.instance_id.0,
                generation: address.session.generation.0,
            }),
            provider_identity_present: record.provider_identity_present,
            updated_at_unix_ms: record.updated_at_unix_ms,
            // Filled in by `fill_managed_session_blocked_stats` against the
            // harness's own observation projection, never by this node-side
            // mapping -- the node's `C2ManagedSessionRecord` carries no such
            // field at all, and this function stays a pure, side-effect-free
            // projection of it (see this function's own doc comment).
            blocked_count: 0,
            last_blocked_at_ms: None,
        }
    }).collect();
    HarnessRuntimeInventoryV1 {
        enabled_providers,
        workspaces,
        workspace_count: inventory.workspace_count,
        workspaces_truncated: inventory.workspaces_truncated,
        session_count: inventory.session_count,
        sessions_truncated: inventory.sessions_truncated,
        managed_sessions,
        managed_session_count: inventory.managed_session_count,
        managed_sessions_truncated: inventory.managed_sessions_truncated,
        launch_inventory: inventory.launch_inventory.map(redact_launch_inventory),
    }
}

/// Overlays `blocked_count`/`last_blocked_at_ms` onto every managed session
/// in a `RuntimeInventoryList` reply from the harness's own observation
/// projection -- the `runtime-inventory` counterpart of `observation_state`
/// (`read.rs`), which does the same lookup-by-`ManagedSessionKey` for a
/// single run's freshness/availability. `redact_runtime_inventory` cannot
/// fill these itself: it is a pure mapping of the node's own
/// `SlimNodeInventory`, which carries nothing about the harness's own
/// `ActionBlocked` observations (see that function's own doc comment).
///
/// A node id, incarnation id, or record id that fails to parse back into
/// its typed form is left at the zero/`None` `redact_runtime_inventory`
/// already set -- every value here round-tripped through validated wire
/// types on the way in, so a parse failure is unreachable in practice, not
/// a case worth surfacing as an error to an operator asking a read-only
/// question.
fn fill_managed_session_blocked_stats(
    page: &mut HarnessRuntimeInventoryPageV1,
    observation: &ObservationService,
) {
    for node in &mut page.nodes {
        let Ok(node_id) = NodeId::new(node.node_id.as_str()) else { continue; };
        let Ok(incarnation_id) = node.incarnation_id.parse::<NodeIncarnationId>() else {
            continue;
        };
        for record in &mut node.inventory.managed_sessions {
            let Ok(record_id) = SessionRecordId::new(record.record_id.as_str()) else {
                continue;
            };
            let key = ManagedSessionKey { node_id: node_id.clone(), incarnation_id, record_id };
            let Some(projection) = observation.projection(&ObservationTarget::Managed { key })
            else {
                continue;
            };
            record.blocked_count = projection.blocked_count;
            record.last_blocked_at_ms = projection.last_blocked_at_ms;
        }
    }
}

fn redact_launch_inventory(
    inventory: gate4agent_node_protocol::LaunchInventory,
) -> HarnessRuntimeLaunchInventoryV1 {
    HarnessRuntimeLaunchInventoryV1 {
        spawn_profiles: inventory.spawn_profiles.map(|profiles| {
            profiles.into_iter().map(|profile| HarnessRuntimeSpawnProfileSummaryV1 {
                id: profile.id.as_str().to_owned(),
                revision: profile.revision.as_str().to_owned(),
                environment_profile: profile.environment_profile.map(|receipt| {
                    HarnessRuntimeEnvironmentProfileReceiptV1 {
                        profile_id: receipt.profile_id.as_str().to_owned(),
                        profile_revision: receipt.profile_revision.as_str().to_owned(),
                    }
                }),
            }).collect()
        }),
        bundles: inventory.bundles.map(|bundles| {
            bundles.into_iter().map(|bundle| HarnessRuntimeBundleReceiptV1 {
                id: bundle.id.as_str().to_owned(),
                revision: bundle.revision.as_str().to_owned(),
                digest: bundle.digest.as_str().to_owned(),
            }).collect()
        }),
    }
}

fn operator_credential_digest(
    credential: &HarnessOperatorCredential,
) -> Result<[u8; 32], HarnessRuntimeError> {
    local_hmac_sha256(
        OPERATOR_CREDENTIAL_DIGEST_DOMAIN,
        credential.expose().as_bytes(),
    ).map_err(|_| HarnessRuntimeError::OperatorCredentialDigest)
}

fn authorize_operator_intent(
    intent: HarnessOperatorIntentV1,
) -> Result<HarnessOperatorRequestV1, HarnessOperatorHostErrorV1> {
    intent.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    let operation_digest = local_hmac_sha256(
        OPERATOR_INTENT_OPERATION_ID_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let idempotency_digest = local_hmac_sha256(
        OPERATOR_INTENT_IDEMPOTENCY_REF_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let task_digest = local_hmac_sha256(
        OPERATOR_INTENT_TASK_ID_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let authority = gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!(
            "hop_{}",
            encode_hex(&operation_digest[..12]),
        )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            encode_hex(&idempotency_digest[..12]),
        )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        actor_id: HarnessSelectorV1::new("harness-operator")
            .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        now_unix_ms: intent.submitted_at_unix_ms,
    };
    let create_task_id = gate4agent_harness_protocol::HarnessTaskId::new(format!(
        "htask_{}",
        encode_hex(&task_digest[..12]),
    )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let request = intent.authorize(authority, create_task_id);
    request.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    Ok(request)
}

fn task_launch_options(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    launch_catalog: &HarnessLaunchCatalog,
    delivery_catalog: &DeliveryCatalogV2,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    task_id: &gate4agent_harness_protocol::HarnessTaskId,
) -> Result<HarnessTaskLaunchOptionsV1, HarnessOperatorHostErrorV1> {
    let task = harness.engine().task(task_id)
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    let (effective_launch, catalog_truncated) =
        effective_launch_catalog(launch_catalog, runtime_inventory);
    if catalog_truncated {
        tracing::warn!(
            "launch catalog exceeds HARNESS_LAUNCH_CATALOG_MAX -- some derived launch plans \
             were dropped from the CLI catalog's remaining capacity",
        );
    }
    if effective_launch.is_empty() {
        tracing::warn!(
            task_id = task_id.as_str(),
            "no launch plans available -- the CLI catalog and the derived runtime inventory \
             are both empty, starting this task would fail",
        );
    }
    let mut plans = effective_launch.ordinary_plans()
        .filter(|plan| matches!(plan.worktree, HarnessWorktreeIntentV1::Existing))
        .map(|plan| {
            if !launch_catalog.contains(&plan.plan_id) {
                tracing::info!(
                    plan_id = plan.plan_id.as_str(),
                    node_id = plan.node_id.as_str(),
                    provider = plan.provider.as_str(),
                    "derived launch plan surfaced in task launch options",
                );
            }
            Ok(HarnessOrdinaryLaunchPlanOptionV1 {
                plan: plan.plan_ref()?,
                node_id: plan.node_id.clone(),
                source_workspace_id: plan.workspace_id.clone(),
                provider_profile: plan.provider_profile.clone(),
                provider_id: HarnessSelectorV1::new(plan.provider.as_str())?,
                mode: plan.mode,
            })
        })
        .collect::<Result<Vec<_>, HarnessServiceError>>()
        .map_err(map_operator_service_error)?;
    let mut truncated = plans.len() > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX;
    plans.truncate(gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX);

    let mut managed_worktree_profiles = runtime_inventory.managed_worktree_profiles.values()
        .flat_map(|cache| cache.profiles.iter())
        .filter(|profile| plans.iter().any(|plan| {
            plan.node_id == profile.node_id
                && plan.source_workspace_id == profile.source_workspace_id
        }))
        .cloned()
        .collect::<Vec<_>>();
    managed_worktree_profiles.sort_by(|left, right| {
        (
            &left.node_id,
            &left.source_workspace_id,
            &left.profile_id,
            &left.profile_revision,
        ).cmp(&(
            &right.node_id,
            &right.source_workspace_id,
            &right.profile_id,
            &right.profile_revision,
        ))
    });
    managed_worktree_profiles.dedup_by(|left, right| {
        left.node_id == right.node_id
            && left.source_workspace_id == right.source_workspace_id
            && left.profile_id == right.profile_id
            && left.profile_revision == right.profile_revision
    });
    truncated |= managed_worktree_profiles.len()
        > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX;
    truncated |= runtime_inventory.managed_worktree_profiles.values()
        .any(|cache| cache.truncated);
    managed_worktree_profiles.truncate(
        gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX,
    );

    let mut context_sources = Vec::new();
    for run in harness.engine().runs() {
        if let Some(source) = context_source_option(
            harness,
            observation,
            support,
            runtime_inventory,
            run,
        )? {
            context_sources.push(source);
        }
    }
    context_sources.sort_by(|left, right| {
        (&left.source_run_id, left.source_run_revision)
            .cmp(&(&right.source_run_id, right.source_run_revision))
    });
    truncated |= context_sources.len() > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX;
    context_sources.truncate(gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX);

    let mut delivery_bundles = delivery_catalog.iter().map(|(bundle_id, compiled)| {
        let selector = HarnessSelectorV1::new(bundle_id.as_str())?;
        crate::delivery::compiled_bundle_selection(selector, compiled)
    }).collect::<Result<Vec<_>, HarnessServiceError>>()
        .map_err(map_operator_service_error)?;
    truncated |= delivery_bundles.len() > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX;
    delivery_bundles.truncate(gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX);

    let current_issued_spec = harness.engine().task_execution_spec_v2(task_id).map(|spec| {
        HarnessIssuedExecutionSpecSummaryV1 {
            task_id: spec.task_id.clone(),
            execution_spec_id: spec.execution_spec_id.clone(),
            revision: spec.revision,
            launch_issuance: spec.launch_issuance.clone(),
            review_policy: spec.review_policy,
            created_at_unix_ms: spec.created_at_unix_ms,
            updated_at_unix_ms: spec.updated_at_unix_ms,
        }
    });
    let mut options = HarnessTaskLaunchOptionsV1 {
        task_id: task.task_id.clone(),
        task_revision: task.revision,
        policy_digest: HarnessRequestDigest::new("0".repeat(64))
            .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        plans,
        managed_worktree_profiles,
        context_sources,
        delivery_bundles,
        current_issued_spec,
        truncated,
    };
    options.policy_digest = crate::task_launch_policy_digest(&options)
        .map_err(map_operator_service_error)?;
    options.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(options)
}

fn context_source_option(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> Result<Option<HarnessContextSourceSelectionV1>, HarnessOperatorHostErrorV1> {
    if let Some(pack) = &run.context_pack {
        let Some(binding) = &run.binding else { return Ok(None); };
        let HarnessSessionIdentityV1::Managed { record_id, .. } = &binding.session else {
            return Ok(None);
        };
        // The durable pack outlives the Node incarnation it was produced
        // under (surviving exactly that restart is the entire point of a
        // Durable source), but `binding.node_incarnation` is `run`'s own
        // original spawn-accept binding and never updates. Re-resolve the
        // Node's CURRENT incarnation by node_id alone here (unlike the Live
        // branch below, which legitimately needs an exact incarnation match
        // since a live session cannot survive a restart either way) — this
        // is what both the later C2 route (`start_context_pack_export`,
        // c2.rs) and the eventual target binding
        // (`HarnessContinuationV1.target_binding`, which
        // `HarnessContinuationV1::validate()`'s `exact_route` invariant also
        // requires to share this same incarnation) actually need to match.
        // `HarnessService::start_task_v2` synthesizes `continuation.source_binding`
        // with this same current incarnation rather than copying `run`'s own
        // stale one verbatim, for the identical reason.
        // If the Node isn't currently known at all, there is no route to
        // resolve the pack through right now — the source is correctly
        // unavailable, not merely stale, until the Node reconnects.
        let Some(current_node_incarnation) = runtime_inventory.nodes.values()
            .find(|node| node.node_id == binding.node_id.as_str())
            .map(|node| node.incarnation_id.clone())
        else {
            return Ok(None);
        };
        let mut source = HarnessContextSourceSelectionV1 {
            source_run_id: run.run_id.clone(),
            source_run_revision: run.revision,
            observed_at_unix_ms: run.updated_at_unix_ms,
            metadata_digest: HarnessRequestDigest::new("0".repeat(64))
                .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
            node_id: binding.node_id.clone(),
            node_incarnation: HarnessSelectorV1::new(current_node_incarnation)
                .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
            workspace_id: binding.workspace_id.clone(),
            session_record_id: record_id.clone(),
            active_session: None,
            message_count: pack.source_message_count,
            message_count_exact: true,
            completed_turn_count: None,
            total_tokens: None,
            availability: HarnessContextSourceAvailabilityV1::Durable,
            context_pack: Some(pack.clone()),
        };
        source.metadata_digest = crate::context_source_metadata_digest(&source)
            .map_err(map_operator_service_error)?;
        source.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        return Ok(Some(source));
    }
    if run.lifecycle != HarnessRunLifecycleV1::Running { return Ok(None); }
    let Some(binding) = &run.binding else { return Ok(None); };
    let HarnessSessionIdentityV1::Managed {
        record_id,
        active_session: Some(active_session),
    } = &binding.session else { return Ok(None); };
    let Some(node) = runtime_inventory.nodes.values().find(|node| {
        node.node_id == binding.node_id.as_str()
            && node.incarnation_id == binding.node_incarnation.as_str()
    }) else { return Ok(None); };
    let Some(_record) = node.inventory.managed_sessions.iter().find(|record| {
        record.record_id == record_id.as_str()
            && record.workspace_id == binding.workspace_id.as_str()
            && record.active_binding.as_ref().is_some_and(|active| {
                active.workspace_id == binding.workspace_id.as_str()
                    && active.instance_id == active_session.instance_id
                    && active.generation == active_session.generation
            })
    }) else { return Ok(None); };
    let monitor = match execute_operator_monitor(harness, observation, support, &run.run_id) {
        Ok(monitor) => monitor,
        Err(_) => return Ok(None),
    };
    if monitor.availability != ProjectionAvailabilityV1::Current
        || monitor.freshness != ProjectionFreshnessV1::Live
        || monitor.transport_incomplete
        || monitor.features.history != FeatureObservationStateV1::Observed
    {
        return Ok(None);
    }
    let Some(history) = monitor.history else { return Ok(None); };
    if history.message_count == 0 || !history.message_count_exact { return Ok(None); }
    let mut source = HarnessContextSourceSelectionV1 {
        source_run_id: run.run_id.clone(),
        source_run_revision: run.revision,
        observed_at_unix_ms: node.observed_at_unix_ms,
        metadata_digest: HarnessRequestDigest::new("0".repeat(64))
            .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        node_id: binding.node_id.clone(),
        node_incarnation: binding.node_incarnation.clone(),
        workspace_id: binding.workspace_id.clone(),
        session_record_id: record_id.clone(),
        active_session: Some(active_session.clone()),
        message_count: history.message_count,
        message_count_exact: history.message_count_exact,
        completed_turn_count: history.completed_turn_count,
        total_tokens: history.total_tokens,
        availability: HarnessContextSourceAvailabilityV1::Live,
        context_pack: None,
    };
    source.metadata_digest = crate::context_source_metadata_digest(&source)
        .map_err(map_operator_service_error)?;
    source.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(Some(source))
}

fn execute_operator_request(
    harness: &mut HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    launch_catalog: &HarnessLaunchCatalog,
    delivery_catalog: &DeliveryCatalogV2,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    terminal_buffers: &TerminalBufferRegistry,
    request: HarnessOperatorRequestV1,
) -> Result<HarnessOperatorResponseV1, HarnessOperatorHostErrorV1> {
    request.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    let request = match request {
        HarnessOperatorRequestV1::SubmitIntent { intent } => authorize_operator_intent(intent)?,
        request => request,
    };
    let response = match request {
        HarnessOperatorRequestV1::MonitorGet { run_id } => {
            HarnessOperatorResponseV1::Monitor(
                execute_operator_monitor(harness, observation, support, &run_id)
                    .map_err(map_operator_read_error)?,
            )
        }
        HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence,
            limit,
        } => HarnessOperatorResponseV1::Timeline(
            execute_operator_timeline(
                harness,
                observation,
                support,
                &run_id,
                after_sequence,
                limit,
            ).map_err(map_operator_read_error)?,
        ),
        HarnessOperatorRequestV1::TasksList { after_task_id, state, limit } => {
            let mut tasks = harness.engine().tasks()
                .filter(|task| {
                    after_task_id.as_ref().map_or(true, |after| &task.task_id > after)
                })
                .filter(|task| state.map_or(true, |state| task.state == state))
                .map(redact_operator_task)
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = tasks.len() > usize::from(limit);
            if has_more { tasks.pop(); }
            let next_cursor = has_more.then(|| {
                tasks.last().expect("nonzero operator page limit").task_id.clone()
            });
            HarnessOperatorResponseV1::Tasks(TaskPageV1 { tasks, next_cursor })
        }
        HarnessOperatorRequestV1::TaskGet { task_id } => {
            let task = harness.engine().task(&task_id)
                .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
            HarnessOperatorResponseV1::Task(redact_operator_task(task))
        }
        HarnessOperatorRequestV1::RunsList {
            task_id,
            after_run_id,
            lifecycle,
            limit,
        } => {
            let mut runs = harness.engine().runs()
                .filter(|run| {
                    after_run_id.as_ref().map_or(true, |after| &run.run_id > after)
                })
                .filter(|run| task_id.as_ref().map_or(true, |task_id| &run.task_id == task_id))
                .filter(|run| lifecycle.map_or(true, |lifecycle| run.lifecycle == lifecycle))
                .map(redact_operator_run)
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = runs.len() > usize::from(limit);
            if has_more { runs.pop(); }
            let next_cursor = has_more.then(|| {
                runs.last().expect("nonzero operator page limit").run_id.clone()
            });
            HarnessOperatorResponseV1::Runs(RunPageV1 { runs, next_cursor })
        }
        HarnessOperatorRequestV1::RunGet { run_id } => {
            let run = harness.engine().run(&run_id)
                .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
            HarnessOperatorResponseV1::Run(redact_operator_run(run))
        }
        HarnessOperatorRequestV1::RunCorrelationGet { run_id } => {
            HarnessOperatorResponseV1::RunCorrelation(
                project_operator_run_correlation(harness, runtime_inventory, &run_id)?,
            )
        }
        HarnessOperatorRequestV1::RunTransferGet { run_id } => {
            HarnessOperatorResponseV1::RunTransfer(
                project_operator_run_transfer(harness, &run_id)?,
            )
        }
        HarnessOperatorRequestV1::ReverseAttributionGet { subject } => {
            HarnessOperatorResponseV1::ReverseAttribution(
                project_reverse_attribution(harness, subject)?,
            )
        }
        HarnessOperatorRequestV1::LaunchPlansList { after_plan_id, limit } => {
            let (effective_launch, _truncated) =
                effective_launch_catalog(launch_catalog, runtime_inventory);
            let mut plans = effective_launch.ordinary_plans()
                .filter(|plan| {
                    after_plan_id.as_ref().map_or(true, |after| &plan.plan_id > after)
                })
                .take(usize::from(limit) + 1)
                .map(|plan| {
                    Ok(HarnessLaunchPlanSummaryV1 {
                        scheduled_launch: plan.ordinary_scheduled_ref()?,
                        node_id: plan.node_id.clone(),
                        workspace_id: plan.workspace_id.clone(),
                        worktree: plan.worktree.clone(),
                        provider_profile: plan.provider_profile.clone(),
                        provider_id: HarnessSelectorV1::new(plan.provider.as_str())?,
                        mode: plan.mode,
                    })
                })
                .collect::<Result<Vec<_>, HarnessServiceError>>()
                .map_err(map_operator_service_error)?;
            let has_more = plans.len() > usize::from(limit);
            if has_more { plans.pop(); }
            let next_plan_id = has_more.then(|| {
                plans.last().expect("nonzero launch plan page limit")
                    .scheduled_launch.plan.plan_id.clone()
            });
            HarnessOperatorResponseV1::LaunchPlans(HarnessLaunchPlanPageV1 {
                plans,
                next_plan_id,
            })
        }
        HarnessOperatorRequestV1::TaskExecutionSpecGet { task_id } => {
            HarnessOperatorResponseV1::TaskExecutionSpec(
                harness.task_execution_spec(&task_id).cloned(),
            )
        }
        HarnessOperatorRequestV1::TaskLaunchOptionsGet { task_id } => {
            HarnessOperatorResponseV1::TaskLaunchOptions(task_launch_options(
                harness,
                observation,
                support,
                launch_catalog,
                delivery_catalog,
                runtime_inventory,
                &task_id,
            )?)
        }
        HarnessOperatorRequestV1::RuntimeInventoryList { after_node_id, limit } => {
            let mut page = runtime_inventory.page(after_node_id.as_deref(), limit);
            fill_managed_session_blocked_stats(&mut page, observation);
            HarnessOperatorResponseV1::RuntimeInventory(page)
        }
        HarnessOperatorRequestV1::TerminalRead { session, after_sequence, limit } => {
            let key = terminal_session_key(&session)?;
            let page = terminal_buffers.page(&key, after_sequence, limit)
                .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
            HarnessOperatorResponseV1::TerminalRead(HarnessRuntimeTerminalPageV1 {
                session,
                frames: page.frames.into_iter()
                    .map(terminal_frame_to_wire)
                    .collect(),
                dropped: page.dropped,
                transport_incomplete: page.transport_incomplete,
                next_cursor: page.next_cursor,
            })
        }
        HarnessOperatorRequestV1::CatalogNativeSessions { .. }
        | HarnessOperatorRequestV1::PageNativeSessions { .. }
        | HarnessOperatorRequestV1::PreviewNativeSession { .. }
        | HarnessOperatorRequestV1::ObserveRunContextSource { .. }
        | HarnessOperatorRequestV1::InspectRunWorkspace { .. }
        | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
        | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
        | HarnessOperatorRequestV1::ReadRunGitDiff { .. }
        | HarnessOperatorRequestV1::InspectNodeWorkspace { .. }
        | HarnessOperatorRequestV1::ReadNodeWorkspaceFile { .. }
        | HarnessOperatorRequestV1::ReadNodeGitHistory { .. }
        | HarnessOperatorRequestV1::ReadNodeGitDiff { .. }
        | HarnessOperatorRequestV1::WriteNodeWorkspaceFile { .. }
        | HarnessOperatorRequestV1::CreateNodeWorkspaceFile { .. }
        | HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        HarnessOperatorRequestV1::CreateTask { request } => {
            operator_mutation_response(harness.operator_create_task(request))?
        }
        HarnessOperatorRequestV1::ReplaceTask { request } => {
            operator_mutation_response(harness.operator_replace_task(request))?
        }
        HarnessOperatorRequestV1::MoveTask { request } => {
            operator_mutation_response(harness.operator_move_task(request))?
        }
        HarnessOperatorRequestV1::CancelTask { request } => {
            operator_mutation_response(harness.operator_cancel_task(request))?
        }
        HarnessOperatorRequestV1::RetryTask { request } => {
            operator_mutation_response(harness.operator_retry_task(request))?
        }
        HarnessOperatorRequestV1::ScheduleNext { request } => {
            let (effective_launch, _truncated) =
                effective_launch_catalog(launch_catalog, runtime_inventory);
            if effective_launch.is_empty() {
                tracing::warn!(
                    "no launch plans available -- the CLI catalog and the derived runtime \
                     inventory are both empty, schedule-next would fail",
                );
            }
            HarnessOperatorResponseV1::Schedule(
                harness.schedule_next(
                    &effective_launch,
                    request.authority,
                    request.plan_id.as_ref(),
                ).map_err(map_operator_service_error)?,
            )
        }
        HarnessOperatorRequestV1::ReplaceTaskExecutionSpec { request } => {
            let (effective_launch, _truncated) =
                effective_launch_catalog(launch_catalog, runtime_inventory);
            let outcome = harness.operator_replace_task_execution_spec(
                &effective_launch,
                request,
            ).map_err(map_operator_service_error)?;
            HarnessOperatorResponseV1::ExecutionSpecMutation(match outcome {
                HarnessApplyOutcome::Applied => HarnessOperatorMutationOutcomeV1::Applied,
                HarnessApplyOutcome::Replayed => HarnessOperatorMutationOutcomeV1::Replayed,
            })
        }
        HarnessOperatorRequestV1::StartTask { request } => {
            let (effective_launch, _truncated) =
                effective_launch_catalog(launch_catalog, runtime_inventory);
            HarnessOperatorResponseV1::TaskStarted(
                harness.start_task(&effective_launch, request)
                    .map_err(map_operator_service_error)?,
            )
        }
        HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 { request } => {
            let options = task_launch_options(
                harness,
                observation,
                support,
                launch_catalog,
                delivery_catalog,
                runtime_inventory,
                &request.task_id,
            )?;
            let outcome = harness.operator_replace_task_execution_spec_v2(
                &options,
                request,
            ).map_err(map_operator_service_error)?;
            HarnessOperatorResponseV1::ExecutionSpecMutation(match outcome {
                HarnessApplyOutcome::Applied => HarnessOperatorMutationOutcomeV1::Applied,
                HarnessApplyOutcome::Replayed => HarnessOperatorMutationOutcomeV1::Replayed,
            })
        }
        HarnessOperatorRequestV1::StartTaskV2 { request } => {
            let options = task_launch_options(
                harness,
                observation,
                support,
                launch_catalog,
                delivery_catalog,
                runtime_inventory,
                &request.task_id,
            )?;
            let (effective_launch, _truncated) =
                effective_launch_catalog(launch_catalog, runtime_inventory);
            HarnessOperatorResponseV1::TaskStarted(
                harness
                    .start_task_v2(&effective_launch, &options, request)
                    .map_err(map_operator_service_error)?,
            )
        }
        // Unreachable in production: the host select loop intercepts these
        // fourteen verbs above this function (see `is_session_spawn_request`/
        // `is_session_control_request` in the `HostCommand::Operator` arm) —
        // this function is synchronous with no C2/adapter handle in scope,
        // so it cannot dispatch a spawn or session-control round trip
        // itself. The arms exist only so this match stays exhaustive.
        HarnessOperatorRequestV1::SpawnSession { .. }
        | HarnessOperatorRequestV1::WriteSessionInput { .. }
        | HarnessOperatorRequestV1::PromptSession { .. }
        | HarnessOperatorRequestV1::ResizeSession { .. }
        | HarnessOperatorRequestV1::StopSession { .. }
        | HarnessOperatorRequestV1::ControlSession { .. }
        | HarnessOperatorRequestV1::WriteSessionBytes { .. }
        | HarnessOperatorRequestV1::PasteSession { .. }
        | HarnessOperatorRequestV1::RemoveSession { .. }
        | HarnessOperatorRequestV1::ResumeSession { .. }
        | HarnessOperatorRequestV1::ResolveInteraction { .. }
        | HarnessOperatorRequestV1::SetSessionMode { .. }
        | HarnessOperatorRequestV1::SetSessionConfigOption { .. }
        | HarnessOperatorRequestV1::SetSessionModel { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        // Same reasoning as the session verbs above: the host select
        // loop intercepts the session-record read (`is_native_history_
        // request`) and mutation (`is_session_record_mutation_request`)
        // families before this function ever sees them.
        HarnessOperatorRequestV1::PreviewSessionRecord { .. }
        | HarnessOperatorRequestV1::ResumeSessionRecord { .. }
        | HarnessOperatorRequestV1::RenameSessionRecord { .. }
        | HarnessOperatorRequestV1::SetSessionTask { .. }
        | HarnessOperatorRequestV1::ForgetSessionRecord { .. }
        | HarnessOperatorRequestV1::IndexProviderSession { .. }
        | HarnessOperatorRequestV1::IndexNativeSession { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        // Same reasoning again: the host select loop intercepts the host-
        // directory-browse read (`is_host_directory_browse_request`) and the
        // resource-mutation family (`is_resource_mutation_request`) before
        // this function ever sees them.
        HarnessOperatorRequestV1::BrowseHostDirectories { .. }
        | HarnessOperatorRequestV1::RegisterWorkspace { .. }
        | HarnessOperatorRequestV1::UnregisterWorkspace { .. }
        | HarnessOperatorRequestV1::CreateStandaloneWorkspace { .. }
        | HarnessOperatorRequestV1::CreateWorktree { .. }
        | HarnessOperatorRequestV1::RemoveWorktree { .. }
        | HarnessOperatorRequestV1::ExportContextPack { .. }
        | HarnessOperatorRequestV1::ForgetContextPack { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        HarnessOperatorRequestV1::SubmitIntent { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        // Unreachable in production: `handle_connection` intercepts
        // `SubscribeEvents` before it is ever wrapped into
        // `HostCommand::Operator` (it registers via `HostCommand::Subscribe`
        // instead -- see that branch's doc comment). This arm exists only so
        // this match stays exhaustive.
        HarnessOperatorRequestV1::SubscribeEvents {} => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        // Same reasoning as `SubscribeEvents` immediately above:
        // `handle_connection` intercepts `SubscribeTerminal` before it is
        // ever wrapped into `HostCommand::Operator` (it registers via
        // `HostCommand::SubscribeTerminal` instead). This arm exists only so
        // this match stays exhaustive.
        HarnessOperatorRequestV1::SubscribeTerminal { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        // Same reasoning as `SubscribeTerminal` immediately above, for its
        // own separate connection: `handle_connection` intercepts
        // `SubscribeAgentStream` before it is ever wrapped into
        // `HostCommand::Operator` (it registers via `HostCommand::
        // SubscribeAgentStream` instead). This arm exists only so this
        // match stays exhaustive.
        HarnessOperatorRequestV1::SubscribeAgentStream { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
    };
    response.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(response)
}

fn terminal_session_key(
    session: &HarnessRuntimeSessionAddressV1,
) -> Result<RuntimeSessionKey, HarnessOperatorHostErrorV1> {
    Ok(RuntimeSessionKey {
        node_id: NodeId::new(session.node_id.as_str())
            .map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?,
        incarnation_id: session.incarnation_id.as_str().parse()
            .map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?,
        workspace_id: gate4agent_node_protocol::WorkspaceId::new(session.workspace_id.as_str())
            .map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?,
        instance_id: gate4agent_types::AgentInstanceId(session.instance_id),
        generation: gate4agent_types::SessionGeneration(session.generation),
    })
}

/// Dispatch-time refusal for `PromptSession` against a session this
/// harness's own cached runtime inventory already knows is PTY-transport --
/// see `HarnessOperatorRequestV1::PromptSession`'s doc comment in
/// `gate4agent-harness-api` for the rule this enforces and why it lives
/// here rather than in the node or in that wire type itself. Called from
/// the `HostCommand::Operator` select-loop arm before `PreparedSessionControl
/// ::from_operator_request` ever runs, so a confirmed-PTY target never
/// reaches C2.
///
/// Returns `None` (proceed to dispatch) both when the cache confirms a
/// non-PTY transport AND when it cannot answer at all (an unparseable
/// address, or the node/incarnation/workspace/session missing from this
/// cache). A miss is a legitimate, expected outcome here, not a defect:
/// `HarnessRuntimeInventoryCache` is explicitly invalidated by the very
/// spawn or resume that creates the session `PromptSession` most wants to
/// target (see `invalidate_runtime_inventory_for_route`'s own doc comment)
/// and is only repopulated by that route's next resync, plus each
/// workspace's own session list can legitimately truncate
/// (`HarnessRuntimeWorkspaceV1::validate`). A fail-CLOSED policy on a miss
/// would make `PromptSession` spuriously refuse exactly the "spawn, then
/// prompt" sequence this verb exists for.
///
/// Failing open through a miss does not open a hole in the owner's PTY
/// rule: every operator-wire path that hands out a live session address
/// leaves an ordinary PTY session's node-side binding `RawPty`-only
/// (`SpawnSession` carries no prompt field at all), so the node's own
/// `require_session_runtime_policy(SemanticPrompt)` gate inside its
/// `NodeRequest::Prompt` handler still refuses an ordinary PTY session
/// that slips past this best-effort check during that window -- just with
/// the generic, provider-and-transport-agnostic `HarnessOperatorHostErrorV1
/// ::UnsupportedCapability` bucket (from `NodeFailureCode::
/// UnsupportedCapability`) instead of the transport/provider-named
/// `UnsupportedTransport` refusal a cache hit gives here. (This gate is PTY-
/// only: an ACP or inline session's own `Prompt`/`Paste` refusal is a
/// different fact entirely -- a turn already in flight, `NodeFailureCode::
/// TurnInFlight` -- and reaches the operator as `Conflict` via
/// `map_session_control_error`, never through this PTY-only pre-check.)
fn prompt_session_pty_refusal(
    runtime_inventory: &HarnessRuntimeInventoryCache,
    session: &HarnessRuntimeSessionAddressV1,
) -> Option<HarnessOperatorHostErrorV1> {
    let key = terminal_session_key(session).ok()?;
    let (transport, provider) = runtime_inventory.session_transport(&key)?;
    (transport == HarnessRuntimeTransportV1::Pty).then(|| {
        HarnessOperatorHostErrorV1::UnsupportedTransport { agent: provider, transport }
    })
}

fn project_reverse_attribution(
    harness: &HarnessService,
    subject: HarnessReverseAttributionSubjectV1,
) -> Result<HarnessReverseAttributionV1, HarnessOperatorHostErrorV1> {
    let requested_workspace = match &subject {
        HarnessReverseAttributionSubjectV1::ManagedRecord { workspace, .. }
        | HarnessReverseAttributionSubjectV1::RuntimeSession { workspace, .. }
        | HarnessReverseAttributionSubjectV1::Workspace { workspace }
        | HarnessReverseAttributionSubjectV1::FileScope { workspace, .. }
        | HarnessReverseAttributionSubjectV1::CommitScope { workspace, .. } => workspace,
    };
    let mut links = Vec::new();
    for run in harness.engine().runs() {
        run.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        let task = harness.engine().task(&run.task_id)
            .filter(|task| task.run_ids.binary_search(&run.run_id).is_ok())
            .ok_or(HarnessOperatorHostErrorV1::Internal)?;
        let Some(binding) = &run.binding else { continue; };
        binding.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        if binding.node_id != requested_workspace.node_id
            || binding.node_incarnation.as_str()
                != requested_workspace.node_incarnation_id.as_str()
            || binding.workspace_id != requested_workspace.workspace_id
        {
            continue;
        }
        let projected = match (&subject, &binding.session) {
            (
                HarnessReverseAttributionSubjectV1::ManagedRecord { record_id, .. },
                HarnessSessionIdentityV1::Managed {
                    record_id: bound_record_id,
                    active_session,
                },
            ) if bound_record_id == record_id => Some((
                HarnessReverseAttributionBindingV1::ManagedRecord {
                    workspace: requested_workspace.clone(),
                    record_id: bound_record_id.clone(),
                    active_instance_id: active_session.as_ref().map(|active| active.instance_id),
                    active_generation: active_session.as_ref().map(|active| active.generation),
                },
                HarnessReverseAttributionRelationV1::ManagedRecordBinding,
            )),
            (
                HarnessReverseAttributionSubjectV1::RuntimeSession {
                    instance_id,
                    generation,
                    ..
                },
                HarnessSessionIdentityV1::Managed {
                    active_session: Some(active),
                    ..
                },
            ) if active.instance_id == *instance_id && active.generation == *generation => Some((
                HarnessReverseAttributionBindingV1::RuntimeSession {
                    workspace: requested_workspace.clone(),
                    instance_id: active.instance_id,
                    generation: active.generation,
                },
                HarnessReverseAttributionRelationV1::RuntimeSessionBinding,
            )),
            (HarnessReverseAttributionSubjectV1::Workspace { .. }, _) => Some((
                HarnessReverseAttributionBindingV1::Workspace {
                    workspace: requested_workspace.clone(),
                },
                HarnessReverseAttributionRelationV1::WorkspaceBinding,
            )),
            (
                HarnessReverseAttributionSubjectV1::FileScope { .. }
                    | HarnessReverseAttributionSubjectV1::CommitScope { .. },
                _,
            ) => Some((
                HarnessReverseAttributionBindingV1::Workspace {
                    workspace: requested_workspace.clone(),
                },
                HarnessReverseAttributionRelationV1::WorkspaceScope,
            )),
            _ => None,
        };
        let Some((binding, relation)) = projected else { continue; };
        links.push(HarnessReverseAttributionLinkV1 {
            task_id: task.task_id.clone(),
            run_id: run.run_id.clone(),
            run_revision: run.revision,
            binding,
            relation,
        });
    }
    links.sort();
    links.dedup();
    links.truncate(gate4agent_harness_api::HARNESS_REVERSE_ATTRIBUTION_LINKS_MAX);
    let outcome = if links.is_empty() {
        HarnessReverseAttributionOutcomeV1::Unattributed
    } else {
        HarnessReverseAttributionOutcomeV1::Attributed
    };
    let response = HarnessReverseAttributionV1 { subject, outcome, links };
    response.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(response)
}

fn operator_mutation_response(
    result: Result<HarnessApplyOutcome, HarnessServiceError>,
) -> Result<HarnessOperatorResponseV1, HarnessOperatorHostErrorV1> {
    let outcome = match result.map_err(map_operator_service_error)? {
        HarnessApplyOutcome::Applied => HarnessOperatorMutationOutcomeV1::Applied,
        HarnessApplyOutcome::Replayed => HarnessOperatorMutationOutcomeV1::Replayed,
    };
    Ok(HarnessOperatorResponseV1::Mutation(outcome))
}

fn map_operator_service_error(error: HarnessServiceError) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessServiceError::Validation(_) => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessServiceError::DispatchPolicy(_) => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessServiceError::InvalidTaskLaunchSelection => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessServiceError::Engine(gate4agent_harness_engine::HarnessEngineError::NotFound(_)) => {
            HarnessOperatorHostErrorV1::NotFound
        }
        HarnessServiceError::ExecutionSpecMissing => HarnessOperatorHostErrorV1::NotFound,
        HarnessServiceError::SchedulerBusy => HarnessOperatorHostErrorV1::Busy,
        HarnessServiceError::Poisoned
        | HarnessServiceError::Store(_)
        | HarnessServiceError::Json(_)
        | HarnessServiceError::Corrupt(_)
        | HarnessServiceError::MutationDigest(_) => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Conflict,
    }
}

fn map_operator_read_error(error: HarnessReadHostErrorV1) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessReadHostErrorV1::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessReadHostErrorV1::NotFoundOrDenied => HarnessOperatorHostErrorV1::NotFound,
        HarnessReadHostErrorV1::TooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessReadHostErrorV1::Deadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessReadHostErrorV1::Unauthorized | HarnessReadHostErrorV1::Internal => {
            HarnessOperatorHostErrorV1::Internal
        }
    }
}

fn redact_operator_task(
    task: &gate4agent_harness_protocol::HarnessTaskV1,
) -> RedactedTaskV1 {
    RedactedTaskV1 {
        task_id: task.task_id.clone(),
        revision: task.revision,
        title: task.title.clone(),
        body: task.body.clone(),
        creator: match task.creator {
            HarnessActorV1::User { .. } => TaskCreatorCategoryV1::User,
            HarnessActorV1::ParentRun { .. } => TaskCreatorCategoryV1::ParentRun,
        },
        parent_task_id: task.parent_task_id.clone(),
        dependency_ids: task.dependencies.clone(),
        state: task.state,
        run_ids: task.run_ids.clone(),
        references_redacted: false,
        result_refs: task.result_refs.clone(),
        artifact_refs: task.artifact_refs.clone(),
        created_at_unix_ms: task.created_at_unix_ms,
        updated_at_unix_ms: task.updated_at_unix_ms,
    }
}

fn redact_operator_run(
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> RedactedRunV1 {
    let binding = match run.binding.as_ref().map(|binding| &binding.session) {
        None => RedactedBindingStateV1::None,
        Some(HarnessSessionIdentityV1::Managed { active_session: None, .. }) => {
            RedactedBindingStateV1::ManagedDormant
        }
        Some(HarnessSessionIdentityV1::Managed { active_session: Some(_), .. }) => {
            RedactedBindingStateV1::ManagedActive
        }
        Some(HarnessSessionIdentityV1::Inline { .. }) => RedactedBindingStateV1::Inline,
    };
    RedactedRunV1 {
        run_id: run.run_id.clone(),
        revision: run.revision,
        parent_run_id: run.parent_run_id.clone(),
        task_id: Some(run.task_id.clone()),
        operation_id: Some(run.operation_id.clone()),
        intent: RedactedRunIntentV1 {
            mode: run.intent.mode,
            worktree: match run.intent.worktree {
                HarnessWorktreeIntentV1::Existing => RedactedWorktreeIntentV1::Existing,
                HarnessWorktreeIntentV1::Managed { .. }
                | HarnessWorktreeIntentV1::ManagedProfile { .. } => {
                    RedactedWorktreeIntentV1::Managed
                }
            },
            has_delivery_bundle: run.intent.delivery_bundle.is_some(),
            has_continuation: run.intent.continuation.is_some(),
        },
        lifecycle: run.lifecycle,
        binding,
        result_disposition: run.result_disposition,
        failure_category: run.failure.as_ref().map(|failure| failure.category),
        context_pack: run.context_pack.clone(),
        git_facts: run.git_facts.clone(),
        references_redacted: false,
        created_at_unix_ms: run.created_at_unix_ms,
        updated_at_unix_ms: run.updated_at_unix_ms,
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    commands: mpsc::Sender<HostCommand>,
    operator_authority: Option<HarnessOperatorCredentialAuthority>,
    connection_permit: tokio::sync::OwnedSemaphorePermit,
    subscriber_connections: Arc<Semaphore>,
    terminal_subscriber_connections: Arc<Semaphore>,
    agent_stream_subscriber_connections: Arc<Semaphore>,
) -> Result<(), HarnessRuntimeError> {
    let mut operator_frame = false;
    // Populated only by the `SubscribeEvents` branch below. Read after the
    // `timeout(HOST_CONNECTION_DEADLINE, ...)` wrapper resolves: a
    // subscription must not sit under that deadline for its whole (unbounded)
    // lifetime, only for the classify-and-register handshake leading up to
    // it -- see the branch's own doc comment.
    let mut subscription: Option<(
        mpsc::Receiver<HarnessOperatorEventV1>,
        tokio::sync::OwnedSemaphorePermit,
    )> = None;
    // Sibling to `subscription` above, populated only by the
    // `SubscribeTerminal` branch below -- kept as its own local (not folded
    // into an enum with `subscription`) because the three are never more
    // than one `Some` at once for the same connection (a connection is
    // classified as one subscription kind or another, or neither) and
    // keeping them physically separate mirrors `TerminalSubscriberRegistry`'s
    // own physical separation from `SubscriberRegistry`.
    let mut terminal_subscription: Option<(
        mpsc::Receiver<HarnessOperatorTerminalEventV1>,
        tokio::sync::OwnedSemaphorePermit,
    )> = None;
    // Sibling to `terminal_subscription` immediately above, populated only
    // by the `SubscribeAgentStream` branch below -- same "own physical
    // local, never both `Some`" reasoning, mirroring
    // `AgentStreamSubscriberRegistry`'s own physical separation from both
    // `SubscriberRegistry` and `TerminalSubscriberRegistry`.
    let mut agent_stream_subscription: Option<(
        mpsc::Receiver<HarnessOperatorAgentEventV1>,
        tokio::sync::OwnedSemaphorePermit,
    )> = None;
    let outcome = timeout(HOST_CONNECTION_DEADLINE, async {
        let frame = match timeout(
            HOST_DEADLINE,
            read_single_frame_detecting_operator(&mut stream, &mut operator_frame),
        ).await {
            Ok(result) => result?,
            Err(_) => {
                // The first-frame read deadline (not the outer connection
                // deadline) elapsed. `operator_frame` may already have been
                // flipped by a partial read that contained "g4aho_" before
                // the timeout fired, so it still picks the correct reply
                // shape — matching the other Deadline branches below, which
                // all write a reply before returning.
                if operator_frame {
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error {
                            error: HarnessOperatorHostErrorV1::Deadline,
                        },
                    ).await?;
                } else {
                    write_reply(
                        &mut stream,
                        HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
                    ).await?;
                }
                return Err(HarnessRuntimeError::Deadline);
            }
        };
        operator_frame = frame_is_operator(&frame);
        if operator_frame {
            let envelope: HarnessOperatorEnvelopeV1 = match serde_json::from_slice(&frame) {
                Ok(envelope) => envelope,
                Err(_) => {
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error {
                            error: HarnessOperatorHostErrorV1::InvalidRequest,
                        },
                    ).await?;
                    return Err(HarnessRuntimeError::InvalidFrame);
                }
            };
            if let Err(error) = envelope.validate() {
                let host_error = match error {
                    HarnessOperatorApiError::WireVersionMismatch { expected, received } => {
                        tracing::warn!(
                            expected,
                            received,
                            "harness operator wire version mismatch: rebuild and restart \
                             the out-of-date side",
                        );
                        HarnessOperatorHostErrorV1::WireVersionMismatch { expected, received }
                    }
                    _ => HarnessOperatorHostErrorV1::InvalidRequest,
                };
                write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error { error: host_error },
                ).await?;
                return Err(HarnessRuntimeError::InvalidFrame);
            }
            let HarnessOperatorEnvelopeV1 { credential, request, .. } = envelope;
            let response_deadline = operator_response_deadline(&request);
            let identity = OperatorRequestLogIdentity::describe(&request);
            let authorized = match operator_authority.as_ref() {
                Some(authority) => authority.verify(&credential)?,
                None => false,
            };
            drop(credential);
            if !authorized {
                write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::Unauthorized,
                    },
                ).await?;
                return Ok(());
            }
            // `SubscribeEvents` branches out of the ordinary one-shot
            // oneshot/deadline path entirely: it registers via
            // `HostCommand::Subscribe` (a bounded `mpsc` sender, not a
            // `oneshot::Sender<HarnessOperatorReplyV1>` -- see
            // `operator_response_deadline`'s doc comment for why a single
            // finite `Duration` cannot bound an open-ended push stream) and
            // returns immediately; the mandatory first `SnapshotBaseline`
            // arrives through that same channel moments later, forwarded by
            // `run_operator_event_subscription` once this whole classify
            // block resolves. Recorded into the outer `subscription` local
            // (captured by mutable reference, the same way `operator_frame`
            // already is) rather than returned directly, so the connection's
            // whole subsequent forwarding-loop lifetime runs outside
            // `HOST_CONNECTION_DEADLINE`.
            if matches!(request, HarnessOperatorRequestV1::SubscribeEvents {}) {
                let Ok(subscriber_permit) = subscriber_connections.clone().try_acquire_owned()
                else {
                    tracing::info!(
                        limit = HOST_SUBSCRIBER_LIMIT,
                        "harness operator event subscribe rejected: subscriber limit reached",
                    );
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Busy },
                    ).await?;
                    return Ok(());
                };
                let (sender, receiver) = mpsc::channel(HOST_SUBSCRIBER_QUEUE_CAPACITY);
                commands.send(HostCommand::Subscribe { sender, identity }).await
                    .map_err(|_| HarnessRuntimeError::HostStopped)?;
                subscription = Some((receiver, subscriber_permit));
                return Ok(());
            }
            // Same "opens a long-lived, server-push subscription" shape as
            // `SubscribeEvents` immediately above, its own connection pool
            // (`terminal_subscriber_connections`), its own registration
            // command (`HostCommand::SubscribeTerminal`), and its own
            // outbound-channel size (`HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY`
            // -- deliberately not `HOST_SUBSCRIBER_QUEUE_CAPACITY`, see that
            // constant's own doc comment in `terminal.rs`). Each requested
            // session address is parsed with the same `terminal_session_key`
            // helper `TerminalRead` already uses; the first one that fails
            // local shape validation fails the whole subscribe with
            // `InvalidRequest`, mirroring how a single malformed field fails
            // any other operator request wholesale rather than admitting a
            // partial subscription.
            if let HarnessOperatorRequestV1::SubscribeTerminal { sessions } = &request {
                let Ok(terminal_subscriber_permit) =
                    terminal_subscriber_connections.clone().try_acquire_owned()
                else {
                    tracing::info!(
                        limit = HOST_TERMINAL_SUBSCRIBER_LIMIT,
                        "harness terminal event subscribe rejected: subscriber limit reached",
                    );
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Busy },
                    ).await?;
                    return Ok(());
                };
                let mut keys = HashSet::with_capacity(sessions.len());
                for session in sessions {
                    let Ok(key) = terminal_session_key(session) else {
                        write_operator_reply(
                            &mut stream,
                            HarnessOperatorReplyV1::Error {
                                error: HarnessOperatorHostErrorV1::InvalidRequest,
                            },
                        ).await?;
                        return Ok(());
                    };
                    keys.insert(key);
                }
                let (sender, receiver) = mpsc::channel(HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY);
                commands.send(HostCommand::SubscribeTerminal {
                    sender,
                    sessions: keys,
                    identity,
                }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
                terminal_subscription = Some((receiver, terminal_subscriber_permit));
                return Ok(());
            }
            // Same "opens a long-lived, server-push subscription" shape as
            // `SubscribeTerminal` immediately above, its own connection pool
            // (`agent_stream_subscriber_connections`), its own registration
            // command (`HostCommand::SubscribeAgentStream`), and its own
            // outbound-channel size (`HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY`
            // -- see that constant's own doc comment in `agent_stream.rs` for
            // why it follows `HOST_SUBSCRIBER_QUEUE_CAPACITY`'s FIFO-with-
            // `Lagged` sizing rather than `HOST_TERMINAL_SUBSCRIBER_QUEUE_
            // CAPACITY`'s coalescing one). Each requested session address is
            // parsed with the same `terminal_session_key` helper -- not
            // terminal-specific despite its name, just the
            // `HarnessRuntimeSessionAddressV1 -> RuntimeSessionKey` parse
            // every session-address-scoped verb on this wire shares -- and
            // the same "first malformed field fails the whole subscribe"
            // discipline as `SubscribeTerminal`.
            if let HarnessOperatorRequestV1::SubscribeAgentStream { sessions } = &request {
                let Ok(agent_stream_subscriber_permit) =
                    agent_stream_subscriber_connections.clone().try_acquire_owned()
                else {
                    tracing::info!(
                        limit = HOST_AGENT_STREAM_SUBSCRIBER_LIMIT,
                        "harness agent stream subscribe rejected: subscriber limit reached",
                    );
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Busy },
                    ).await?;
                    return Ok(());
                };
                let mut keys = HashSet::with_capacity(sessions.len());
                for session in sessions {
                    let Ok(key) = terminal_session_key(session) else {
                        write_operator_reply(
                            &mut stream,
                            HarnessOperatorReplyV1::Error {
                                error: HarnessOperatorHostErrorV1::InvalidRequest,
                            },
                        ).await?;
                        return Ok(());
                    };
                    keys.insert(key);
                }
                let (sender, receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
                commands.send(HostCommand::SubscribeAgentStream {
                    sender,
                    sessions: keys,
                    identity,
                }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
                agent_stream_subscription = Some((receiver, agent_stream_subscriber_permit));
                return Ok(());
            }
            // A node-workspace-read, node-workspace-write, session-spawn,
            // session-control, session-record-mutation, host-directory-
            // browse, or resource-mutation request gets a cancel signal:
            // these are the request families whose worker can run an
            // unbounded C2 round trip behind it (see `start_node_workspace_
            // read_worker`/`start_node_workspace_write_worker`/
            // `start_session_spawn_worker`/`start_session_control_worker`/
            // `start_session_record_mutation_worker`/`start_host_directory_
            // browse_worker`/`start_resource_mutation_worker`). `cancel_tx`
            // fires explicitly on the deadline branch below, and is also
            // dropped (equivalent to firing) on every other early return past
            // this point, including the outer `HOST_CONNECTION_DEADLINE`
            // cutoff wrapping this whole block.
            let needs_cancel_signal = is_node_workspace_read_request(&request)
                || is_node_workspace_write_request(&request)
                || is_session_spawn_request(&request)
                || is_session_control_request(&request)
                || is_session_record_mutation_request(&request)
                || is_host_directory_browse_request(&request)
                || is_resource_mutation_request(&request);
            let mut cancel_tx = None;
            let cancel_rx = if needs_cancel_signal {
                let (tx, rx) = oneshot::channel();
                cancel_tx = Some(tx);
                Some(rx)
            } else {
                None
            };
            let (reply, receive) = oneshot::channel();
            commands.send(HostCommand::Operator {
                request,
                reply,
                cancel: cancel_rx,
            }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
            let reply = match timeout(response_deadline, receive).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => return Err(HarnessRuntimeError::HostStopped),
                Err(_) => {
                    if let Some(cancel_tx) = cancel_tx {
                        let _ = cancel_tx.send(());
                    }
                    tracing::warn!(
                        operation = %identity.operation,
                        node_id = identity.node_id(),
                        workspace_id = identity.workspace_id(),
                        run_id = identity.run_id(),
                        deadline_ms = response_deadline.as_millis() as u64,
                        "operator request exceeded its response deadline",
                    );
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error {
                            error: HarnessOperatorHostErrorV1::Deadline,
                        },
                    ).await?;
                    return Err(HarnessRuntimeError::Deadline);
                }
            };
            return match write_operator_reply(&mut stream, reply).await {
                Err(HarnessRuntimeError::ResponseTooLarge) => write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::TooLarge,
                    },
                ).await,
                result => result,
            };
        }
        let envelope: HarnessReadEnvelopeV1 = serde_json::from_slice(&frame)
            .map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        envelope.validate().map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        let (reply, receive) = oneshot::channel();
        commands.send(HostCommand::Read { envelope, reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        let reply = match timeout(HOST_DEADLINE, receive).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(HarnessRuntimeError::HostStopped),
            Err(_) => {
                write_reply(
                    &mut stream,
                    HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
                ).await?;
                return Err(HarnessRuntimeError::Deadline);
            }
        };
        match write_reply(&mut stream, reply).await {
            Err(HarnessRuntimeError::ResponseTooLarge) => write_reply(
                &mut stream,
                HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::TooLarge },
            ).await,
            result => result,
        }
    }).await;
    match outcome {
        // `subscription`/`terminal_subscription`/`agent_stream_subscription`
        // are mutually exclusive -- exactly one of the three classify
        // branches above can have set any of them, never more than one --
        // so matching the triple together stays exhaustive without a
        // reachable-but-impossible fourth-or-more combination to reason
        // about.
        Ok(Ok(())) => match (subscription, terminal_subscription, agent_stream_subscription) {
            (Some((receiver, subscriber_permit)), _, _) => {
                // The main connection permit is released here, not held for
                // the subscription's whole (unbounded) lifetime: see
                // `HOST_SUBSCRIBER_LIMIT`'s doc comment. `subscriber_permit`
                // takes over as the thing keeping this connection counted
                // against a limit for as long as it stays open.
                drop(connection_permit);
                run_operator_event_subscription(stream, receiver, subscriber_permit)
                    .await
            }
            (None, Some((receiver, terminal_subscriber_permit)), _) => {
                // Same reasoning as the task/run/node branch above, against
                // `HOST_TERMINAL_SUBSCRIBER_LIMIT` instead.
                drop(connection_permit);
                run_operator_terminal_subscription(stream, receiver, terminal_subscriber_permit)
                    .await
            }
            (None, None, Some((receiver, agent_stream_subscriber_permit))) => {
                // Same reasoning again, against
                // `HOST_AGENT_STREAM_SUBSCRIBER_LIMIT` instead.
                drop(connection_permit);
                run_operator_agent_stream_subscription(
                    stream,
                    receiver,
                    agent_stream_subscriber_permit,
                ).await
            }
            (None, None, None) => Ok(()),
        },
        Ok(Err(error)) => Err(error),
        Err(_) => {
            if operator_frame {
                let _ = write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::Deadline,
                    },
                ).await;
            } else {
                let _ = write_reply(
                    &mut stream,
                    HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
                ).await;
            }
            Err(HarnessRuntimeError::Deadline)
        }
    }
}

/// Forwarding loop for a connection that just subscribed
/// (`handle_connection`'s `SubscribeEvents` branch): repeatedly receives
/// from `events` and writes each one as its own newline-terminated JSON line
/// (`write_operator_event`) -- deliberately outside
/// `HOST_CONNECTION_DEADLINE`, so a subscription lives exactly as long as
/// the client keeps the socket open, not 45s. Ends -- dropping
/// `subscriber_permit`, which frees this connection's slot in the dedicated
/// subscriber pool -- the moment a write fails or `events` closes (the
/// select loop pruned this subscriber: its sender's `Closed` outcome in
/// `SubscriberRegistry::emit`/`recover_lagged`, or the host shutting down).
/// There is no unsubscribe frame to read for: the framing this connection
/// negotiated has no representable second client-to-host message (see the
/// module doc), so the client's own `Drop` for its subscription handle is
/// exactly "close the socket," which this loop observes as a write failure
/// on its very next attempt.
///
/// Promoted `pub` for `gate4agent-harness-light` (A3): this forwarding loop
/// has no kernel dependency of its own (a bare `TcpStream` + `mpsc::Receiver`
/// + `OwnedSemaphorePermit`), so it is reused verbatim for that crate's own
/// `SubscribeEvents` connections rather than reimplemented.
pub async fn run_operator_event_subscription(
    mut stream: TcpStream,
    mut events: mpsc::Receiver<HarnessOperatorEventV1>,
    _subscriber_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), HarnessRuntimeError> {
    tracing::info!("operator event subscription forwarding started");
    while let Some(event) = events.recv().await {
        tracing::debug!(kind = event_kind_label(&event), "forwarding operator event");
        write_operator_event(&mut stream, event).await?;
    }
    tracing::info!("operator event subscription channel drained; closing");
    Ok(())
}

fn event_kind_label(event: &HarnessOperatorEventV1) -> &'static str {
    match event {
        HarnessOperatorEventV1::SnapshotBaseline { .. } => "snapshot-baseline",
        HarnessOperatorEventV1::TaskChanged { .. } => "task-changed",
        HarnessOperatorEventV1::RunChanged { .. } => "run-changed",
        HarnessOperatorEventV1::RuntimeInventoryChanged { .. } => "runtime-inventory-changed",
        HarnessOperatorEventV1::RuntimeInventoryRemoved { .. } => "runtime-inventory-removed",
        HarnessOperatorEventV1::Lagged { .. } => "lagged",
        HarnessOperatorEventV1::Ping { .. } => "ping",
    }
}

/// Push-frame counterpart to `write_operator_reply`: writes one
/// newline-terminated `HarnessOperatorEventV1` and flushes, but -- unlike
/// `write_operator_reply` -- never shuts the connection down afterward, so
/// the socket stays open for the next event.
///
/// Promoted `pub` for `gate4agent-harness-light` (A3), same reasoning as
/// `run_operator_event_subscription` (its one caller) above.
pub async fn write_operator_event(
    stream: &mut TcpStream,
    event: HarnessOperatorEventV1,
) -> Result<(), HarnessRuntimeError> {
    event.validate().map_err(|error| {
        // A push frame failing its own validation is a server-side bug; the
        // stream dies here, so the cause must not die silently with it.
        tracing::warn!(
            error = ?error,
            kind = event_kind_label(&event),
            "operator event failed validation before write; closing the subscription",
        );
        HarnessRuntimeError::InvalidReply
    })?;
    let mut encoded = serde_json::to_vec(&event).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_OPERATOR_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.flush().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

/// Forwarding loop for a connection that just subscribed via
/// `SubscribeTerminal` (`handle_connection`'s own branch above) --
/// `run_operator_event_subscription`'s exact sibling, over
/// `HarnessOperatorTerminalEventV1`/`terminal_subscriber_permit` instead:
/// same "outside `HOST_CONNECTION_DEADLINE`, ends on the first write failure
/// or channel close" lifetime, same reasoning for why there is no
/// unsubscribe frame to read for (see that function's own doc comment,
/// which applies verbatim here).
pub async fn run_operator_terminal_subscription(
    mut stream: TcpStream,
    mut events: mpsc::Receiver<HarnessOperatorTerminalEventV1>,
    _subscriber_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), HarnessRuntimeError> {
    tracing::info!("terminal event subscription forwarding started");
    while let Some(event) = events.recv().await {
        tracing::debug!(
            kind = terminal_event_kind_label(&event),
            "forwarding terminal event",
        );
        write_operator_terminal_event(&mut stream, event).await?;
    }
    tracing::info!("terminal event subscription channel drained; closing");
    Ok(())
}

fn terminal_event_kind_label(event: &HarnessOperatorTerminalEventV1) -> &'static str {
    match event {
        HarnessOperatorTerminalEventV1::TerminalFrame { .. } => "terminal-frame",
        HarnessOperatorTerminalEventV1::Ping { .. } => "ping",
    }
}

/// Push-frame counterpart to `write_operator_reply` for a `SubscribeTerminal`
/// connection -- `write_operator_event`'s exact sibling over
/// `HarnessOperatorTerminalEventV1` instead (same validate-before-write,
/// same size ceiling, same "never shuts the connection down afterward"
/// contract).
pub async fn write_operator_terminal_event(
    stream: &mut TcpStream,
    event: HarnessOperatorTerminalEventV1,
) -> Result<(), HarnessRuntimeError> {
    event.validate().map_err(|error| {
        // Same reasoning as `write_operator_event`'s own validate-before-write
        // check: a push frame failing its own validation is a server-side
        // bug, so the stream dies here rather than the cause dying silently
        // with it.
        tracing::warn!(
            error = ?error,
            kind = terminal_event_kind_label(&event),
            "terminal event failed validation before write; closing the subscription",
        );
        HarnessRuntimeError::InvalidReply
    })?;
    let mut encoded = serde_json::to_vec(&event).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_OPERATOR_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.flush().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

/// Upper bound on how many already-queued agent-stream events one forwarder
/// write batches together -- see `run_operator_agent_stream_subscription`'s
/// own doc comment for why. Mirrors `NODE_CONNECTION_EVENT_BURST_MAX`'s own
/// per-tick burst-cap reasoning (`server.rs`): sized well under
/// `HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY` so one write never tries to
/// drain the entire queue in a single shot, which would let one slow batch
/// hold up every other subscriber's turn on this same task's event loop.
const AGENT_STREAM_FORWARD_BATCH_MAX: usize = 32;

/// Forwarding loop for a connection that just subscribed via
/// `SubscribeAgentStream` (`handle_connection`'s own branch above) --
/// `run_operator_terminal_subscription`'s exact sibling, over
/// `HarnessOperatorAgentEventV1`/`agent_stream_subscriber_permit` instead:
/// same "outside `HOST_CONNECTION_DEADLINE`, ends on the first write failure
/// or channel close" lifetime, same reasoning for why there is no
/// unsubscribe frame to read for (see `run_operator_event_subscription`'s
/// own doc comment, which applies verbatim here).
///
/// Drains `events` in bounded batches (`AGENT_STREAM_FORWARD_BATCH_MAX`)
/// rather than one `write_all`+`flush` per event: a burst of small `Text`
/// deltas otherwise pays one write-syscall round trip per delta, and if the
/// operator socket is even briefly slow to accept writes, `events.recv()` is
/// not called again while that single write blocks -- the producer-side
/// queue (`HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY`) backs up purely
/// from this task's own per-event write overhead, not from the client
/// actually falling behind. One write per already-queued batch removes that
/// self-inflicted stall without changing anything about backpressure once
/// the queue is genuinely empty (falls straight back to `events.recv().await`
/// blocking, exactly as before).
pub async fn run_operator_agent_stream_subscription(
    mut stream: TcpStream,
    mut events: mpsc::Receiver<HarnessOperatorAgentEventV1>,
    _subscriber_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), HarnessRuntimeError> {
    tracing::info!("agent stream event subscription forwarding started");
    while let Some(first) = events.recv().await {
        let mut batch = Vec::with_capacity(1);
        batch.push(first);
        while batch.len() < AGENT_STREAM_FORWARD_BATCH_MAX {
            match events.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        write_operator_agent_stream_events(&mut stream, batch).await?;
    }
    tracing::info!("agent stream event subscription channel drained; closing");
    Ok(())
}

fn agent_stream_event_kind_label(event: &HarnessOperatorAgentEventV1) -> &'static str {
    match event {
        HarnessOperatorAgentEventV1::AgentChunk { .. } => "agent-chunk",
        HarnessOperatorAgentEventV1::Lagged { .. } => "lagged",
        HarnessOperatorAgentEventV1::Ping { .. } => "ping",
    }
}

/// Validates and newline-frames one `HarnessOperatorAgentEventV1`, shared by
/// `write_operator_agent_stream_event` (one event, one write) and
/// `write_operator_agent_stream_events` (a batch, one write for all of
/// them) below -- same validate-before-write, same size ceiling, same
/// warn-and-close-on-failure either way.
fn encode_operator_agent_stream_event(
    event: HarnessOperatorAgentEventV1,
) -> Result<Vec<u8>, HarnessRuntimeError> {
    event.validate().map_err(|error| {
        // Same reasoning as `write_operator_terminal_event`'s own
        // validate-before-write check: a push frame failing its own
        // validation is a server-side bug, so the stream dies here rather
        // than the cause dying silently with it.
        tracing::warn!(
            error = ?error,
            kind = agent_stream_event_kind_label(&event),
            "agent stream event failed validation before write; closing the subscription",
        );
        HarnessRuntimeError::InvalidReply
    })?;
    let mut encoded = serde_json::to_vec(&event).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_OPERATOR_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    Ok(encoded)
}

/// Push-frame counterpart to `write_operator_reply` for a
/// `SubscribeAgentStream` connection -- `write_operator_terminal_event`'s
/// exact sibling over `HarnessOperatorAgentEventV1` instead (same
/// validate-before-write, same size ceiling, same "never shuts the
/// connection down afterward" contract).
pub async fn write_operator_agent_stream_event(
    stream: &mut TcpStream,
    event: HarnessOperatorAgentEventV1,
) -> Result<(), HarnessRuntimeError> {
    let encoded = encode_operator_agent_stream_event(event)?;
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.flush().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

/// Validates and encodes every event in `events` exactly the way the
/// single-event path does (same per-event `validate()` call, same
/// warn-and-close-on-failure, same per-event `"forwarding agent stream
/// event"` debug log), concatenating every encoded frame into ONE buffer --
/// the pure half of `write_operator_agent_stream_events` below, split out
/// so the batching contract itself (N queued events collapse into one
/// buffer, in order) is directly testable without needing a real socket's
/// write-syscall count to observe it.
fn encode_operator_agent_stream_event_batch(
    events: Vec<HarnessOperatorAgentEventV1>,
) -> Result<Vec<u8>, HarnessRuntimeError> {
    let mut encoded = Vec::new();
    for event in events {
        tracing::debug!(
            kind = agent_stream_event_kind_label(&event),
            "forwarding agent stream event",
        );
        encoded.append(&mut encode_operator_agent_stream_event(event)?);
    }
    Ok(encoded)
}

/// Batch counterpart used by `run_operator_agent_stream_subscription`'s
/// forwarding loop: issues ONE `write_all`+`flush` for the whole buffer
/// `encode_operator_agent_stream_event_batch` returns -- see that
/// function's own doc comment, and `run_operator_agent_stream_subscription`'s,
/// for why.
async fn write_operator_agent_stream_events(
    stream: &mut TcpStream,
    events: Vec<HarnessOperatorAgentEventV1>,
) -> Result<(), HarnessRuntimeError> {
    let encoded = encode_operator_agent_stream_event_batch(events)?;
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.flush().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

fn frame_is_operator(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame).ok()
        .and_then(|value| value.get("credential")?.as_str().map(str::to_owned))
        .is_some_and(|credential| credential.starts_with("g4aho_"))
}

#[cfg(test)]
async fn read_single_frame(stream: &mut TcpStream) -> Result<Vec<u8>, HarnessRuntimeError> {
    let mut operator_frame = false;
    read_single_frame_detecting_operator(stream, &mut operator_frame).await
}

/// Reads one newline-delimited request frame off `stream`, bounded by
/// `HARNESS_READ_REQUEST_MAX_BYTES`, flipping `*operator_frame` to `true`
/// the moment any read chunk contains the `g4aho_` operator-credential
/// prefix (a cheap streaming classification used by [`handle_connection`]'s
/// deadline-branch reply-shape choice; a caller that only ever serves
/// operator frames, like `gate4agent-harness-light`, can pass a throwaway
/// `&mut bool` and ignore it).
///
/// Promoted `pub` for `gate4agent-harness-light`: the light harness serves
/// the identical newline-delimited-JSON operator wire this function already
/// frames for the full harness, and reimplementing this exact byte-level
/// read loop (size cap, single-frame/no-embedded-newline validation, EOF
/// handling) would only risk the two hosts silently drifting apart on
/// framing while adding nothing light mode needs to do differently.
pub async fn read_single_frame_detecting_operator(
    stream: &mut TcpStream,
    operator_frame: &mut bool,
) -> Result<Vec<u8>, HarnessRuntimeError> {
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|_| HarnessRuntimeError::ReadFailed)?;
        if read == 0 { break; }
        if bytes.len().saturating_add(read) > HARNESS_READ_REQUEST_MAX_BYTES {
            return Err(HarnessRuntimeError::RequestTooLarge);
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(b"g4aho_".len()).any(|window| window == b"g4aho_") {
            *operator_frame = true;
        }
    }
    if bytes.len() < 2 || bytes.last() != Some(&b'\n')
        || bytes[..bytes.len() - 1].contains(&b'\n')
    {
        return Err(HarnessRuntimeError::InvalidFrame);
    }
    bytes.pop();
    Ok(bytes)
}

async fn write_reply(
    stream: &mut TcpStream,
    reply: HarnessReadReplyV1,
) -> Result<(), HarnessRuntimeError> {
    reply.validate().map_err(|_| HarnessRuntimeError::InvalidReply)?;
    let mut encoded = serde_json::to_vec(&reply).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_READ_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.shutdown().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

/// Validates, encodes, and writes one newline-terminated
/// `HarnessOperatorReplyV1`, bounded by `HARNESS_OPERATOR_RESPONSE_MAX_BYTES`,
/// then shuts the write half down (the wire's one-request-per-connection
/// framing: EOF-after-reply is the reply boundary, matching
/// [`read_single_frame_detecting_operator`]'s EOF-is-the-request-boundary on
/// the other side).
///
/// Promoted `pub` for `gate4agent-harness-light`, alongside
/// `read_single_frame_detecting_operator`: same framing, same size cap, same
/// consumer.
pub async fn write_operator_reply(
    stream: &mut TcpStream,
    reply: HarnessOperatorReplyV1,
) -> Result<(), HarnessRuntimeError> {
    reply.validate().map_err(|_| HarnessRuntimeError::InvalidReply)?;
    let mut encoded = serde_json::to_vec(&reply).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_OPERATOR_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.shutdown().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObservationSupportRegistry {
    routes: BTreeMap<(NodeId, NodeIncarnationId), RouteObservationAuthority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RouteObservationAuthority {
    support: Option<C2ObservationSupport>,
    healthy: bool,
}

impl ObservationSupportRegistry {
    pub(crate) fn get(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Option<Option<C2ObservationSupport>> {
        self.routes.get(&(node_id.clone(), incarnation_id)).map(|route| route.support)
    }

    pub(crate) fn is_authoritative(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> bool {
        self.routes.get(&(node_id.clone(), incarnation_id))
            .is_some_and(|route| route.healthy)
    }

    fn replace(
        &mut self,
        node_id: NodeId,
        incarnation_id: NodeIncarnationId,
        support: Option<C2ObservationSupport>,
    ) {
        self.routes.insert(
            (node_id, incarnation_id),
            RouteObservationAuthority { support, healthy: true },
        );
    }

    fn mark_unhealthy(&mut self, node_id: &NodeId, incarnation_id: NodeIncarnationId) {
        self.routes.entry((node_id.clone(), incarnation_id))
            .and_modify(|route| route.healthy = false)
            .or_insert(RouteObservationAuthority { support: None, healthy: false });
    }

    fn mark_all_unhealthy(&mut self) {
        for route in self.routes.values_mut() {
            route.healthy = false;
        }
    }

    fn reconcile_current_routes(&mut self, routes: &[NodeRoute]) {
        for ((node_id, incarnation_id), authority) in self.routes.iter_mut() {
            if !routes.iter().any(|route| {
                &route.node_id == node_id
                    && route.expected_incarnation_id == *incarnation_id
            }) {
                authority.healthy = false;
            }
        }
    }
}

pub async fn run_observation_bridge(
    adapter: HarnessC2Adapter,
    mut events: HarnessC2EventReceiver,
    mut observation: ObservationService,
) -> Result<(), HarnessRuntimeError> {
    let mut support = ObservationSupportRegistry::default();
    recover_all_observation_routes(&adapter, &mut observation, &mut support).await?;
    while let Some(event) = events.recv().await {
        apply_live_event(&adapter, &mut observation, &mut support, event).await?;
    }
    observation.flush()?;
    Ok(())
}

pub(crate) async fn recover_all_routes(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
) -> Result<(), HarnessRuntimeError> {
    for route in adapter.observation_routes() {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        if let Err(error) = recover_route(
            adapter,
            harness,
            observation,
            support,
            runtime_inventory,
            &route,
            requested_after,
        ).await {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            if !matches!(error, HarnessRuntimeError::C2(_)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn recover_all_observation_routes(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
) -> Result<(), HarnessRuntimeError> {
    for route in adapter.observation_routes() {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        if let Err(error) = recover_observation_route(
            adapter,
            observation,
            support,
            &route,
            requested_after,
        ).await {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            if !matches!(error, HarnessRuntimeError::C2(_)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub(crate) async fn apply_live_event(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    routed: RoutedNodeEvent,
) -> Result<(), HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let prior = durable_cursor_for(observation, &route);
    if prior.is_none() || routed.cursor.sequence > prior.unwrap_or(0).saturating_add(1) {
        recover_observation_route(
            adapter,
            observation,
            support,
            &route,
            prior.unwrap_or(0),
        ).await?;
    }
    if matches!(routed.event, C2NodeEvent::ResyncRequired { .. }) {
        let current = durable_cursor_for(observation, &route).unwrap_or(0);
        recover_observation_route(adapter, observation, support, &route, current).await?;
    }
    if durable_cursor_for(observation, &route).is_some_and(|sequence| {
        sequence >= routed.cursor.sequence
    }) {
        return Ok(());
    }
    let envelope = routed_event_to_ingress(routed, unix_time_ms())?;
    observation.apply_ingress(envelope)?;
    Ok(())
}

fn apply_or_buffer_host_live_event(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    recovery: &mut ObservationRecoveryRegistry,
    subscribers: &mut SubscriberRegistry,
    routed: RoutedNodeEvent,
) -> Result<(), HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let current_route = match adapter.exact_route(&route.node_id) {
        Ok(current_route) => current_route,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            recovery.remove(&route);
            return Err(error.into());
        }
    };
    if current_route != route {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        recovery.remove(&route);
        return Ok(());
    }
    let prior = durable_cursor_for(observation, &route);
    if prior.is_some_and(|sequence| sequence >= routed.cursor.sequence) {
        return Ok(());
    }
    let inventory_refresh_required = invalidate_runtime_inventory_for_event(
        recovery,
        &route,
        &routed.event,
    );
    let recovery_required = inventory_refresh_required
        || prior.is_none()
        || routed.cursor.sequence > prior.unwrap_or(0).saturating_add(1)
        || matches!(routed.event, C2NodeEvent::ResyncRequired { .. });
    if recovery_required {
        let touch = freeze_bound_route_waiting(
            harness,
            &route,
            routed.cursor.sequence,
            unix_time_ms(),
        )?;
        notify_touched(subscribers, harness, &touch);
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
    }
    let already_recovering = recovery.contains(&route);
    if recovery_required || already_recovering {
        let is_resync_required = matches!(routed.event, C2NodeEvent::ResyncRequired { .. });
        let route_recovery = recovery.ensure_route(route);
        if is_resync_required && already_recovering {
            route_recovery.refresh_after_completion = true;
        }
        route_recovery.buffer(routed);
        return Ok(());
    }
    let received_at = unix_time_ms();
    let lifecycle_touch = apply_exact_control_lifecycle(harness, &routed, received_at)?;
    notify_touched(subscribers, harness, &lifecycle_touch);
    let context_pack_run_ids = apply_live_context_pack_receipt(harness, &route, &routed, received_at)?;
    notify_touched(subscribers, harness, &EngineTouch {
        task_ids: Vec::new(),
        run_ids: context_pack_run_ids,
    });
    if durable_cursor_for(observation, &route).is_some_and(|sequence| {
        sequence >= routed.cursor.sequence
    }) {
        return Ok(());
    }
    observation.apply_ingress(routed_event_to_ingress(routed, received_at)?)?;
    Ok(())
}

fn event_affects_runtime_inventory(event: &C2NodeEvent) -> bool {
    matches!(
        event,
        C2NodeEvent::Control { .. }
            | C2NodeEvent::WorkspaceAdded { .. }
            | C2NodeEvent::WorkspaceRemoved { .. }
            | C2NodeEvent::SessionRecordUpserted { .. }
            | C2NodeEvent::SessionRecordRemoved { .. }
            | C2NodeEvent::ManagedWorktreeUpserted { .. }
            | C2NodeEvent::ManagedWorktreeRemoved { .. }
            | C2NodeEvent::ResyncRequired { .. }
    )
}

fn invalidate_runtime_inventory_for_event(
    recovery: &mut ObservationRecoveryRegistry,
    route: &NodeRoute,
    event: &C2NodeEvent,
) -> bool {
    if !event_affects_runtime_inventory(event) {
        return false;
    }
    invalidate_runtime_inventory_for_route(recovery, route);
    true
}

/// Unconditional counterpart to `invalidate_runtime_inventory_for_event`,
/// with no `C2NodeEvent` to gate on: drops this harness's own cached
/// projection of the node and schedules a targeted resync for `route` the
/// same way the reactive live-event path does, so `start_pending_
/// observation_recoveries`'s very next pass picks it up and
/// `finish_observation_recovery` emits `RuntimeInventoryChanged` through
/// the same Eq-diffed `refresh` -- no direct event construction bypassing
/// that diff.
///
/// **Emits nothing, and takes no `SubscriberRegistry` so that it cannot.**
/// A cache is not a source of truth about whether the node exists: every
/// caller here reaches this function on a mutation that SUCCEEDED against a
/// live node, i.e. at the one moment the node is most certainly present.
/// It used to push `RuntimeInventoryRemoved` whenever the drop actually hit
/// a cached entry, and subscribers cannot tell that apart from the node
/// genuinely leaving the topology -- so the TUI tore down every PTY,
/// preview, file and git tab of a node in the same second a session was
/// spawned on it (`index-native-session` is one of these mutations). Real
/// departure has exactly one producer, the `topology.changed()` arm, which
/// emits per id returned by `HarnessRuntimeInventoryCache::
/// reconcile_topology`; it is the only place that observes departure.
///
/// **Touches no cached projection either, and takes no cache so that it
/// cannot.** Scheduling a resync is the whole job. This used to also DELETE
/// the node's cached entry, which made every reader of that cache --
/// `all_nodes`, `page`, `node`, and so every `RuntimeInventoryList` poll an
/// operator makes -- answer "no such node" for the window between a
/// successful mutation and its resync landing. That window is exactly when
/// a just-spawned session is trying to open, which is why the TUI logged
/// `spawned but its node is not in view yet` on a node that had never gone
/// anywhere. Stale is a legitimate answer and `observed_at_unix_ms` on the
/// projection says how stale; absent is not a way to say it. The resync
/// replaces the entry through `refresh`, whose `Eq` diff then decides
/// whether anything actually changed and is worth an event.
///
/// Exists for callers that know a route needs a fresh resync from context
/// alone, not from a live event that happened to carry the news: today,
/// a successful `StopSession`, a resumed session, a session-record
/// mutation, and an inventory-affecting resource mutation. A forced stop
/// kills the node's PTY process directly and is not guaranteed to
/// round-trip a `SessionRecordUpserted`/`SessionRecordRemoved`/`Control`
/// event back through the live C2 stream the way every other
/// inventory-affecting change does (the asymmetry: session creation
/// reliably publishes one, abrupt termination is not guaranteed to), so the
/// reactive path alone cannot be relied on to notice.
fn invalidate_runtime_inventory_for_route(
    recovery: &mut ObservationRecoveryRegistry,
    route: &NodeRoute,
) {
    let route_recovery = recovery.ensure_route(route.clone());
    if route_recovery.attempt.is_some() {
        route_recovery.refresh_after_completion = true;
    }
}

fn start_pending_observation_recoveries(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    observation: &ObservationService,
    recovery: &mut ObservationRecoveryRegistry,
) {
    let available = OBSERVATION_RECOVERY_MAX_IN_FLIGHT
        .saturating_sub(recovery.in_flight());
    if available == 0 {
        return;
    }
    let now = Instant::now();
    let routes = recovery.routes.values()
        .filter(|route| route.attempt.is_none() && route.retry_after <= now)
        .take(available)
        .map(|route| route.route.clone())
        .collect::<Vec<_>>();
    for route in routes {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        let attempt_id = recovery.allocate_attempt_id();
        let route_recovery = recovery.routes.get_mut(
            &ObservationRecoveryRegistry::key(&route),
        ).expect("selected recovery route must remain registered");
        route_recovery.attempt = Some(ObservationRecoveryAttempt {
            attempt_id,
            requested_after,
        });
        let worker_adapter = adapter.clone();
        let worker_commands = commands.clone();
        tokio::spawn(async move {
            let result = worker_adapter.observation_resync(&route, requested_after).await;
            let _ = worker_commands.send(HostCommand::ObservationRecoveryFinished {
                route,
                attempt_id,
                requested_after,
                result,
            }).await;
        });
    }
}

fn finish_observation_recovery(
    recovery: &mut ObservationRecoveryRegistry,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    subscribers: &mut SubscriberRegistry,
    route: NodeRoute,
    attempt_id: u64,
    requested_after: u64,
    result: Result<HarnessObservationResync, HarnessC2Error>,
) -> Result<(), HarnessRuntimeError> {
    let key = ObservationRecoveryRegistry::key(&route);
    let Some(route_recovery) = recovery.routes.get_mut(&key) else {
        return Ok(());
    };
    if !route_recovery.accepts_completion(&route, attempt_id, requested_after) {
        return Ok(());
    }
    route_recovery.attempt = None;
    let resync = match result {
        Ok(resync) => resync,
        Err(_) => {
            route_recovery.retry_after = Instant::now() + OBSERVATION_RECOVERY_RETRY;
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Ok(());
        }
    };
    if resync.route() != &route
        || resync.requested_after_sequence() != requested_after
    {
        route_recovery.retry_after = Instant::now() + OBSERVATION_RECOVERY_RETRY;
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Ok(());
    }

    let received_at = unix_time_ms();
    let resync_touch = apply_resync_lifecycle(harness, &resync, received_at)?;
    notify_touched(subscribers, harness, &resync_touch);
    commit_observation_resync(observation, support, &resync, received_at)?;
    if let Some(node) = runtime_inventory.refresh(&resync, received_at) {
        subscribers.emit(|sequence| HarnessOperatorEventV1::RuntimeInventoryChanged {
            sequence,
            node: node.clone(),
        });
    }

    let requires_follow_up = route_recovery.overflowed
        || route_recovery.refresh_after_completion;
    if requires_follow_up {
        route_recovery.prepare_follow_up();
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Ok(());
    }

    let buffered = std::mem::take(&mut route_recovery.buffered);
    route_recovery.buffered_bytes = 0;
    let mut follow_up = false;
    for (_, routed) in buffered {
        let prior = durable_cursor_for(observation, &route).unwrap_or(0);
        if prior >= routed.cursor.sequence {
            continue;
        }
        if routed.cursor.sequence > prior.saturating_add(1) {
            follow_up = true;
            break;
        }
        let lifecycle_touch = apply_exact_control_lifecycle(harness, &routed, received_at)?;
        notify_touched(subscribers, harness, &lifecycle_touch);
        let context_pack_run_ids =
            apply_live_context_pack_receipt(harness, &route, &routed, received_at)?;
        notify_touched(subscribers, harness, &EngineTouch {
            task_ids: Vec::new(),
            run_ids: context_pack_run_ids,
        });
        observation.apply_ingress(routed_event_to_ingress(routed, received_at)?)?;
    }
    if follow_up {
        route_recovery.prepare_follow_up();
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Ok(());
    }
    // A resync this structurally clean can still be stale for a route with
    // a pending `awaiting_absent_sessions` expectation -- see that field's
    // doc comment. Re-check against whatever `refresh` just cached (not the
    // `Option` it returned, which is `None` on an unchanged value) and, if
    // any expected-gone session is still there, retry instead of treating
    // this resync as the final word.
    let session_still_present = route_recovery.awaiting_absent_sessions.iter().any(|session| {
        runtime_inventory.node(&route.node_id)
            .is_some_and(|node| node_still_has_session(node, session))
    });
    if session_still_present {
        route_recovery.retry_after = Instant::now() + OBSERVATION_RECOVERY_RETRY;
        return Ok(());
    }
    route_recovery.awaiting_absent_sessions.clear();
    recovery.routes.remove(&key);
    Ok(())
}

fn node_still_has_session(node: &HarnessRuntimeNodeInventoryV1, session: &SessionAddress) -> bool {
    node.inventory.workspaces.get(session.workspace_id.as_str())
        .is_some_and(|workspace| workspace.sessions.iter().any(|candidate| {
            candidate.instance_id == session.session.instance_id.0
                && candidate.generation == session.session.generation.0
        }))
}

struct HarnessMcpRelayPlan {
    route: NodeRoute,
    reservation_id: gate4agent_node_protocol::HarnessMcpReservationId,
    activation_digest: gate4agent_node_protocol::HarnessMcpActivationDigest,
    record_id: gate4agent_node_protocol::SessionRecordId,
    session: gate4agent_node_protocol::SessionAddress,
    call_id: gate4agent_node_protocol::HarnessMcpCallId,
    deadline_unix_ms: u64,
    outcome: Result<Vec<u8>, HarnessMcpRejectReasonV1>,
}

fn prepare_harness_mcp_read_call(
    adapter: &HarnessC2Adapter,
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    routed: RoutedNodeEvent,
) -> Result<HarnessMcpRelayPlan, HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let C2NodeEvent::HarnessMcpReadCall {
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        request,
        deadline_unix_ms,
    } = routed.event else {
        return Err(HarnessRuntimeError::InvalidHarnessMcpEvent);
    };
    let tool_id = crate::read::harness_mcp_tool_id(&request);
    let now = unix_time_ms();
    let current_route = adapter.exact_route(&route.node_id)?;
    let authorization = if current_route != route || now >= deadline_unix_ms {
        Err(HarnessReadHostErrorV1::Unauthorized)
    } else {
        harness.authorize_harness_mcp_call(
            &route,
            &reservation_id,
            &activation_digest,
            &record_id,
            &session,
        ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)
    };
    // Captured before `authorization` is consumed below -- this is the one
    // piece of the served call's identity (`grant id`) that only exists on
    // the success side, since an unauthorized call never resolved a grant
    // at all.
    let grant_id = authorization.as_ref().ok().map(|binding| binding.grant_id.clone());
    let response = authorization.and_then(|binding| {
        verify_observation_credential_binding(observation, support, &binding)?;
        execute_exact_binding_read(harness, observation, support, &binding, request)
    });
    let outcome = match response {
        Ok(response) => {
            let reply = HarnessMcpLocalReplyV1::Ok { response };
            if reply.validate().is_err() {
                Err(HarnessMcpRejectReasonV1::ResponseTooLarge)
            } else {
                let encoded = serde_json::to_vec(&reply)
                    .map_err(|_| HarnessRuntimeError::InvalidReply)?;
                if encoded.len() > MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES {
                    Err(HarnessMcpRejectReasonV1::ResponseTooLarge)
                } else {
                    Ok(encoded)
                }
            }
        }
        Err(error) => Err(reject_reason(error)),
    };
    // One line per served call (not per reply chunk -- chunking happens
    // later, in `relay_harness_mcp_read_call`), naming the grant and
    // session a live proof can cross-check against the MCP client's own
    // `tools/call` trace.
    tracing::info!(
        node_id = route.node_id.as_str(),
        workspace_id = session.workspace_id.as_str(),
        session = ?session.session,
        reservation_id = reservation_id.as_str(),
        call_id = call_id.as_str(),
        tool = tool_id,
        grant_id = grant_id.as_ref().map(|grant_id| grant_id.as_str()).unwrap_or("unauthorized"),
        served = outcome.is_ok(),
        "harness MCP read call served",
    );
    Ok(HarnessMcpRelayPlan {
        route,
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        deadline_unix_ms,
        outcome,
    })
}

async fn relay_harness_mcp_read_call(
    adapter: &HarnessC2Adapter,
    plan: HarnessMcpRelayPlan,
) -> Result<(), HarnessRuntimeError> {
    let HarnessMcpRelayPlan {
        route,
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        deadline_unix_ms,
        outcome,
    } = plan;
    let encoded = match outcome {
        Ok(encoded) => encoded,
        Err(reason) => {
            reject_harness_mcp_before_deadline(
                adapter, &route, &reservation_id, &activation_digest, &record_id,
                &session, &call_id, reason, deadline_unix_ms,
            ).await;
            return Ok(());
        }
    };
    let chunks = encoded.chunks(MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES)
        .collect::<Vec<_>>();
    let mut offset = 0u32;
    for (index, chunk) in chunks.iter().enumerate() {
        let now = unix_time_ms();
        if now >= deadline_unix_ms {
            return Ok(());
        }
        let chunk_hex = HarnessMcpReplyChunkHexV1::new(encode_hex(chunk))
            .map_err(|_| HarnessRuntimeError::InvalidReply)?;
        let remaining = deadline_unix_ms - now;
        offset = match timeout(
            Duration::from_millis(remaining),
            adapter.put_harness_mcp_reply_chunk(
                &route,
                &reservation_id,
                &activation_digest,
                &record_id,
                &session,
                &call_id,
                offset,
                index + 1 == chunks.len(),
                chunk_hex,
            ),
        ).await {
            Ok(result) => result?,
            Err(_) => return Ok(()),
        };
    }
    Ok(())
}

async fn reject_harness_mcp_before_deadline(
    adapter: &HarnessC2Adapter,
    route: &NodeRoute,
    reservation_id: &gate4agent_node_protocol::HarnessMcpReservationId,
    activation_digest: &gate4agent_node_protocol::HarnessMcpActivationDigest,
    record_id: &gate4agent_node_protocol::SessionRecordId,
    session: &gate4agent_node_protocol::SessionAddress,
    call_id: &gate4agent_node_protocol::HarnessMcpCallId,
    reason: HarnessMcpRejectReasonV1,
    deadline_unix_ms: u64,
) {
    let now = unix_time_ms();
    if now >= deadline_unix_ms { return; }
    let _ = timeout(
        Duration::from_millis(deadline_unix_ms - now),
        adapter.reject_harness_mcp_call(
            route,
            reservation_id,
            activation_digest,
            record_id,
            session,
            call_id,
            reason,
        ),
    ).await;
}

fn reject_reason(error: HarnessReadHostErrorV1) -> HarnessMcpRejectReasonV1 {
    match error {
        HarnessReadHostErrorV1::Unauthorized => HarnessMcpRejectReasonV1::Unauthorized,
        HarnessReadHostErrorV1::InvalidRequest => HarnessMcpRejectReasonV1::InvalidRequest,
        HarnessReadHostErrorV1::NotFoundOrDenied => HarnessMcpRejectReasonV1::NotFoundOrDenied,
        HarnessReadHostErrorV1::TooLarge => HarnessMcpRejectReasonV1::ResponseTooLarge,
        HarnessReadHostErrorV1::Deadline => HarnessMcpRejectReasonV1::Deadline,
        HarnessReadHostErrorV1::Internal => HarnessMcpRejectReasonV1::Internal,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

async fn recover_route(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    route: &NodeRoute,
    requested_after: u64,
) -> Result<(), HarnessRuntimeError> {
    let resync = match adapter.observation_resync(route, requested_after).await {
        Ok(resync) => resync,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Err(error.into());
        }
    };
    let received_at_ms = unix_time_ms();
    if let Err(error) = apply_resync_lifecycle(harness, &resync, received_at_ms) {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Err(error);
    }
    commit_observation_resync(observation, support, &resync, received_at_ms)?;
    runtime_inventory.refresh(&resync, received_at_ms);
    Ok(())
}

async fn recover_observation_route(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    route: &NodeRoute,
    requested_after: u64,
) -> Result<(), HarnessRuntimeError> {
    let resync = match adapter.observation_resync(route, requested_after).await {
        Ok(resync) => resync,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Err(error.into());
        }
    };
    commit_observation_resync(observation, support, &resync, unix_time_ms())
}

fn apply_resync_lifecycle(
    harness: &mut HarnessService,
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<EngineTouch, HarnessRuntimeError> {
    let eviction_gap_sequence = resync.has_eviction_gap()
        .then_some(resync.oldest_available_sequence() - 1);
    let mut touch = apply_replayed_lifecycle_events(
        harness,
        resync.route(),
        eviction_gap_sequence,
        resync.lifecycle_control_events(),
        received_at_ms,
    )?;
    touch.merge(apply_snapshot_lifecycle(
        harness,
        resync.route(),
        resync.event_sequence(),
        resync.snapshot(),
        resync.lifecycle_control_events(),
        received_at_ms,
    )?);
    touch.run_ids.extend(apply_snapshot_context_pack_receipts(
        harness,
        resync.route(),
        resync.snapshot(),
        received_at_ms,
    )?);
    Ok(touch)
}

/// Reconciles one more durable fact from a managed session record: its most
/// recent clean-exit ContextPack export (`exported_context`) is copied onto
/// every harness run currently bound to that exact record, on the exact
/// route it was observed on, exactly once. Deliberately matches on every
/// lifecycle, not just `Waiting` — a Completed run is exactly the case A2
/// cares about — and the `run.context_pack.is_some()` guard already makes a
/// repeat pass a no-op; the remaining (run_id, digest) idempotency lives in
/// `record_run_context_pack` itself, so this — and both callers below — can
/// be invoked redundantly at no cost. Independent of and never
/// blocks/delays any lifecycle transition committed elsewhere: if the pack
/// has not landed yet, it simply shows up on a later call (design risk #1
/// — no race to win). Shared by both context-pack receipt appliers: the
/// resync/recovery snapshot scan (`apply_snapshot_context_pack_receipts`,
/// one record at a time from a full snapshot) and the steady-state live
/// reaction (`apply_live_context_pack_receipt`, the one record a
/// `SessionRecordUpserted` event just carried).
fn apply_context_pack_receipt_for_record(
    harness: &mut HarnessService,
    route: &NodeRoute,
    record: &C2ManagedSessionRecord,
    received_at_ms: u64,
) -> Result<Vec<gate4agent_harness_protocol::HarnessRunId>, HarnessRuntimeError> {
    let Some(exported_context) = record.exported_context.as_ref() else {
        return Ok(Vec::new());
    };
    let matches = harness.engine().runs().filter_map(|run| {
        if run.context_pack.is_some() {
            return None;
        }
        let binding = run.binding.as_ref()?;
        let HarnessSessionIdentityV1::Managed { record_id, .. } = &binding.session else {
            return None;
        };
        if binding.node_id.as_str() != route.node_id.as_str()
            || binding.node_incarnation.as_str() != route.expected_incarnation_id.to_string()
            || record_id.as_str() != record.record_id.as_str()
            || binding.workspace_id.as_str() != record.workspace_id.as_str()
        {
            return None;
        }
        Some(run.run_id.clone())
    }).collect::<Vec<_>>();
    for run_id in &matches {
        let receipt = crate::context_receipt_from_node(exported_context)?;
        harness.record_run_context_pack(
            run_id,
            receipt,
            received_at_ms,
            &route.node_id,
            route.expected_incarnation_id,
        )?;
    }
    Ok(matches)
}

/// Resync/recovery path: scans a full snapshot's session records for any
/// carrying an `exported_context`, delegating each one to
/// `apply_context_pack_receipt_for_record`. See that function's doc comment
/// for the shared idempotency and the steady-state counterpart below.
fn apply_snapshot_context_pack_receipts(
    harness: &mut HarnessService,
    route: &NodeRoute,
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
    received_at_ms: u64,
) -> Result<Vec<gate4agent_harness_protocol::HarnessRunId>, HarnessRuntimeError> {
    if snapshot.node_id != route.node_id {
        return Ok(Vec::new());
    }
    let mut touched_run_ids = Vec::new();
    for record in &snapshot.session_records {
        touched_run_ids.extend(
            apply_context_pack_receipt_for_record(harness, route, record, received_at_ms)?,
        );
    }
    Ok(touched_run_ids)
}

/// Steady-state path: reacts to the live `SessionRecordUpserted` event a
/// Node publishes the moment `set_exported_context` commits a record's
/// `exported_context` (`gate4agent-node/src/server.rs`'s
/// `set_exported_context` -> `publish_record`), so a run's own
/// `context_pack` lands within the very same live event that carried the
/// export while the route stays healthy — no reconnect or resync required.
/// Called from `apply_or_buffer_host_live_event` right alongside (and after)
/// `apply_exact_control_lifecycle`, never gated behind the observation
/// cursor dedup check further down: `record_run_context_pack`'s own
/// (run_id, digest) idempotency already makes a redundant call free, exactly
/// as it does for `apply_snapshot_context_pack_receipts`, whose per-record
/// logic this shares via `apply_context_pack_receipt_for_record`.
fn apply_live_context_pack_receipt(
    harness: &mut HarnessService,
    route: &NodeRoute,
    routed: &RoutedNodeEvent,
    received_at_ms: u64,
) -> Result<Vec<gate4agent_harness_protocol::HarnessRunId>, HarnessRuntimeError> {
    let C2NodeEvent::SessionRecordUpserted { record } = &routed.event else {
        return Ok(Vec::new());
    };
    apply_context_pack_receipt_for_record(harness, route, record, received_at_ms)
}

/// Background, best-effort capture of one run's bounded git workspace
/// summary, reusing the exact `InspectRunWorkspace` C2 round trip the live
/// operator Git tab already uses (`PreparedRunRead::for_run`,
/// `HarnessC2Adapter::start_prepared_run_read`) — no second inspection
/// mechanism. Driven by `run_git_facts_sweep`'s periodic tick
/// (`RUN_GIT_FACTS_SWEEP_PERIOD`), not a reactive hook on any lifecycle
/// commit — see the A3 design §3.4 for why. Single-attempt by construction:
/// `git_facts` can only transition `None -> Some(_)` once
/// (`prepare_run_git_facts_record`'s validator, harness-engine), so a run
/// this sweep already attempted (successfully or not) is filtered out here
/// by `run.git_facts.is_some()` and never re-enqueued — no retry storm
/// against a permanently-unavailable workspace (A3 design §11 risk 2).
fn reconcile_run_git_facts_capture(
    harness: &HarnessService,
    adapter: &HarnessC2Adapter,
    workers: &mut RunGitFactsWorkerRegistry,
    commands: &mpsc::Sender<HostCommand>,
) {
    for run in harness.engine().runs() {
        if run.git_facts.is_some() || run.binding.is_none()
            || !matches!(run.lifecycle, HarnessRunLifecycleV1::Completed
                | HarnessRunLifecycleV1::Failed | HarnessRunLifecycleV1::Cancelled)
        {
            continue;
        }
        if !workers.try_start() {
            return;
        }
        let Ok(prepared) = PreparedRunRead::for_run(run, WorkspaceReadKind::InspectWorkspace)
        else {
            workers.finish();
            continue;
        };
        match adapter.start_prepared_run_read(prepared) {
            Ok(pending) => start_run_git_facts_capture_worker(
                pending,
                run.run_id.clone(),
                commands.clone(),
            ),
            Err(_) => workers.finish(), // route/queue-full — try again next tick
        }
    }
}

/// Resolves one finished background git-facts capture attempt
/// (`HostCommand::RunGitFactsCaptureFinished`). Bails silently — no error
/// surfaces anywhere, this is purely a background fact — if the run is
/// gone, its binding no longer matches the frozen `PreparedRunRead` origin
/// (`validate_run_read_completion_origin`, reused as-is), or `git_facts`
/// already landed from a concurrent pass (harmless, matches
/// `apply_context_pack_receipt_for_record`'s own idempotency posture).
/// Otherwise records `Captured` on a well-formed workspace inspection or
/// `Unavailable` on any transport/route/deadline/rejection failure or
/// unexpected response shape, and swallows any engine-level rejection from
/// `record_run_git_facts` itself — a stale-revision race is a legitimate,
/// harmless outcome for a background, single-attempt fact (A3 design §11
/// risk 2: this attempt is never retried, by design).
fn finish_run_git_facts_capture(
    harness: &mut HarnessService,
    run_id: gate4agent_harness_protocol::HarnessRunId,
    completion: RunReadCompletion,
    now_unix_ms: u64,
) {
    let (prepared, result) = completion.into_parts();
    if validate_run_read_completion_origin(harness.engine().run(&run_id), &prepared).is_err() {
        return;
    }
    let Some(current) = harness.engine().run(&run_id) else { return; };
    if current.git_facts.is_some() {
        return;
    }
    let Some(binding) = current.binding.as_ref() else { return; };
    let Ok(node_id) = NodeId::new(binding.node_id.as_str()) else { return; };
    let Ok(incarnation_id) = binding.node_incarnation.as_str().parse::<NodeIncarnationId>()
    else {
        return;
    };
    let outcome = match result {
        Ok(HarnessOperatorResponseV1::RunWorkspaceInspected(inspection)) => {
            HarnessRunGitFactsOutcomeV1::Captured(
                run_git_summary_from_inspection(&inspection.git),
            )
        }
        Ok(_) | Err(_) => HarnessRunGitFactsOutcomeV1::Unavailable,
    };
    let facts = HarnessRunGitFactsV1 { captured_at_unix_ms: now_unix_ms, outcome };
    let _ = harness.record_run_git_facts(&run_id, facts, now_unix_ms, &node_id, incarnation_id);
}

/// Purely-mechanical, infallible field-for-field projection from the
/// operator-facing (`gate4agent-harness-api`) git-summary hierarchy to its
/// structurally-identical `gate4agent-harness-protocol` mirror (A3 design
/// §1.1/§3.2). Infallible because both hierarchies share numerically
/// identical bounds (status entries/recent commits/path/branch/summary byte
/// caps) and `inspection.git` already passed
/// `HarnessOperatorResponseV1::validate()` (`correlate_run_read_response`)
/// before reaching here — a straight copy can never violate the
/// protocol-side `HarnessRunGitSummaryV1::validate()`.
fn run_git_summary_from_inspection(
    git: &gate4agent_harness_api::HarnessGitSummaryV1,
) -> HarnessRunGitSummaryV1 {
    HarnessRunGitSummaryV1 {
        is_repository: git.is_repository,
        branch: git.branch.clone(),
        status: git.status.iter().map(run_git_status_entry_from_api).collect(),
        recent_commits: git.recent_commits.iter()
            .map(run_git_commit_summary_from_api)
            .collect(),
        truncated: git.truncated,
    }
}

fn run_git_status_entry_from_api(
    entry: &gate4agent_harness_api::HarnessGitStatusEntryV1,
) -> HarnessRunGitStatusEntryV1 {
    HarnessRunGitStatusEntryV1 {
        index_status: run_git_status_code_from_api(entry.index_status),
        worktree_status: run_git_status_code_from_api(entry.worktree_status),
        path: entry.path.as_str().to_owned(),
        previous_path: entry.previous_path.as_ref().map(|path| path.as_str().to_owned()),
    }
}

fn run_git_status_code_from_api(
    code: gate4agent_harness_api::HarnessGitStatusCodeV1,
) -> HarnessRunGitStatusCodeV1 {
    match code {
        gate4agent_harness_api::HarnessGitStatusCodeV1::Unmodified => {
            HarnessRunGitStatusCodeV1::Unmodified
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Added => HarnessRunGitStatusCodeV1::Added,
        gate4agent_harness_api::HarnessGitStatusCodeV1::Modified => {
            HarnessRunGitStatusCodeV1::Modified
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Deleted => {
            HarnessRunGitStatusCodeV1::Deleted
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Renamed => {
            HarnessRunGitStatusCodeV1::Renamed
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Copied => HarnessRunGitStatusCodeV1::Copied,
        gate4agent_harness_api::HarnessGitStatusCodeV1::Unmerged => {
            HarnessRunGitStatusCodeV1::Unmerged
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Untracked => {
            HarnessRunGitStatusCodeV1::Untracked
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::Ignored => {
            HarnessRunGitStatusCodeV1::Ignored
        }
        gate4agent_harness_api::HarnessGitStatusCodeV1::TypeChanged => {
            HarnessRunGitStatusCodeV1::TypeChanged
        }
    }
}

fn run_git_commit_summary_from_api(
    commit: &gate4agent_harness_api::HarnessGitCommitSummaryV1,
) -> HarnessRunGitCommitSummaryV1 {
    HarnessRunGitCommitSummaryV1 {
        id: commit.id.as_str().to_owned(),
        summary: commit.summary.clone(),
    }
}

fn apply_snapshot_lifecycle(
    harness: &mut HarnessService,
    route: &NodeRoute,
    event_sequence: u64,
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
    lifecycle_control_events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
    received_at_ms: u64,
) -> Result<EngineTouch, HarnessRuntimeError> {
    if event_sequence == 0 {
        return Ok(EngineTouch::default());
    }
    let matches = harness.engine().runs().filter_map(|run| {
        if run.lifecycle != HarnessRunLifecycleV1::Waiting
            || replay_contains_exact_lifecycle(run, route, lifecycle_control_events)
        {
            return None;
        }
        exact_snapshot_lifecycle(run, route, snapshot).map(|(kind, projection)| {
            (run.run_id.clone(), run.task_id.clone(), kind, projection)
        })
    }).collect::<Vec<_>>();
    let mut touch = EngineTouch::default();
    for (run_id, task_id, kind, projection) in matches {
        commit_lifecycle_projection(
            harness,
            &run_id,
            &route.node_id,
            route.expected_incarnation_id,
            event_sequence,
            kind,
            projection,
            received_at_ms,
        )?;
        touch.run_ids.push(run_id);
        touch.task_ids.push(task_id);
    }
    touch.merge(EngineTouch {
        task_ids: reconcile_task_result_refs(harness, received_at_ms)?,
        run_ids: Vec::new(),
    });
    Ok(touch)
}

fn replay_contains_exact_lifecycle(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    route: &NodeRoute,
    events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
) -> bool {
    events.iter().any(|event| {
        exact_bound_control_lifecycle(
            run,
            &RoutedNodeEvent {
                node_id: route.node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: route.expected_incarnation_id,
                    sequence: event.sequence,
                },
                event: event.event.clone(),
            },
        ).is_some()
    })
}

fn exact_snapshot_lifecycle(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    route: &NodeRoute,
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
) -> Option<(HarnessLifecycleEventKindV1, HarnessLifecycleProjectionV1)> {
    let binding = run.binding.as_ref()?;
    let HarnessSessionIdentityV1::Managed { record_id, active_session: Some(active) } =
        &binding.session
    else {
        return None;
    };
    if binding.node_id.as_str() != route.node_id.as_str()
        || binding.node_incarnation.as_str() != route.expected_incarnation_id.to_string()
        || snapshot.node_id != route.node_id
    {
        return None;
    }
    let mut records = snapshot.session_records.iter().filter(|record| {
        record.record_id.as_str() == record_id.as_str()
            && record.workspace_id.as_str() == binding.workspace_id.as_str()
    });
    let record = records.next()?;
    if records.next().is_some()
        || record.state != gate4agent_node_protocol::ManagedSessionState::Live
        || !snapshot_record_matches_binding(record, binding, active)
    {
        return None;
    }
    let mut sessions = snapshot.workspaces.iter()
        .filter(|workspace| workspace.workspace_id.as_str() == binding.workspace_id.as_str())
        .flat_map(|workspace| workspace.sessions.iter())
        .filter(|session| {
            session.instance_id.0 == active.instance_id
                && session.generation.0 == active.generation
        });
    let status = &sessions.next()?.status;
    if sessions.next().is_some() {
        return None;
    }
    match status {
        C2SessionStatus::Running => Some((
            HarnessLifecycleEventKindV1::Running,
            HarnessLifecycleProjectionV1::Running,
        )),
        C2SessionStatus::Failed => Some((
            HarnessLifecycleEventKindV1::Failed,
            HarnessLifecycleProjectionV1::Failed,
        )),
        C2SessionStatus::Registered
        | C2SessionStatus::Starting
        | C2SessionStatus::Stopping
        | C2SessionStatus::Exited { .. } => None,
    }
}

fn snapshot_record_matches_binding(
    record: &C2ManagedSessionRecord,
    binding: &HarnessSessionBindingV1,
    active: &HarnessRuntimeIdentityV1,
) -> bool {
    record.active_session.as_ref().is_some_and(|address| {
        address.workspace_id.as_str() == binding.workspace_id.as_str()
            && address.session.instance_id.0 == active.instance_id
            && address.session.generation.0 == active.generation
    })
}

fn apply_replayed_lifecycle_events(
    harness: &mut HarnessService,
    route: &NodeRoute,
    eviction_gap_sequence: Option<u64>,
    events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
    received_at_ms: u64,
) -> Result<EngineTouch, HarnessRuntimeError> {
    let mut touch = EngineTouch::default();
    if let Some(gap_sequence) = eviction_gap_sequence {
        touch.merge(freeze_bound_route_waiting(harness, route, gap_sequence, received_at_ms)?);
    }
    for event in events {
        touch.merge(apply_exact_control_lifecycle(
            harness,
            &RoutedNodeEvent {
                node_id: route.node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: route.expected_incarnation_id,
                    sequence: event.sequence,
                },
                event: event.event.clone(),
            },
            received_at_ms,
        )?);
    }
    Ok(touch)
}

fn commit_observation_resync(
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let route = resync.route();
    let batch = observation_resync_batch(resync, received_at_ms)?;
    if let Err(error) = observation.apply_resync(batch) {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Err(error.into());
    }
    support.replace(
        route.node_id.clone(),
        route.expected_incarnation_id,
        resync.observation_support(),
    );
    Ok(())
}

fn observation_resync_batch(
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<ObservationResyncBatch, HarnessRuntimeError> {
    if received_at_ms == 0 {
        return Err(HarnessRuntimeError::ZeroReceiveTime);
    }
    let route = resync.route();
    let support = resync.observation_support();
    let records_complete = support.is_some_and(|support| {
        support.events && support.managed_target
    });
    let records = if records_complete {
        resync.managed_inventory().iter().map(|record| {
            ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    record_id: record.record_id.clone(),
                },
                runtime: record.active_session.as_ref().map(|address| RuntimeSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    workspace_id: address.workspace_id.clone(),
                    instance_id: address.session.instance_id,
                    generation: address.session.generation,
                }),
            }
        }).collect()
    } else {
        Vec::new()
    };
    let gaps = resync.has_eviction_gap().then(|| ObservationGap {
        first_sequence: resync.requested_after_sequence().saturating_add(1),
        last_sequence: resync.oldest_available_sequence() - 1,
    }).into_iter().collect();
    let events = resync.observation_events().iter().cloned().map(|event| {
        routed_event_to_ingress(RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: event.sequence,
            },
            event: event.event,
        }, received_at_ms)
    }).collect::<Result<Vec<_>, _>>()?;
    Ok(ObservationResyncBatch {
        node_id: route.node_id.clone(),
        incarnation_id: route.expected_incarnation_id,
        requested_after: resync.requested_after_sequence(),
        high_watermark: NodeCursor {
            incarnation_id: route.expected_incarnation_id,
            sequence: resync.event_sequence(),
        },
        oldest_available_sequence: resync.oldest_available_sequence(),
        records,
        records_complete,
        gaps,
        events,
    })
}

fn durable_cursor_for(observation: &ObservationService, route: &NodeRoute) -> Option<u64> {
    observation.durable_resume_cursors().into_iter().find_map(|(node_id, cursor)| {
        (node_id == route.node_id && cursor.incarnation_id == route.expected_incarnation_id)
            .then_some(cursor.sequence)
    })
}

fn ensure_current_topology_binding(
    adapter: &HarnessC2Adapter,
    binding: &CredentialBindingV1,
) -> Result<(), HarnessRuntimeError> {
    let node_id = gate4agent_node_protocol::NodeId::new(binding.node_id.as_str())
        .map_err(|_| HarnessRuntimeError::CredentialBinding)?;
    let route = adapter.exact_route(&node_id)
        .map_err(|_| HarnessRuntimeError::CredentialBinding)?;
    if !topology_binding_matches_route(binding, &route) {
        return Err(HarnessRuntimeError::CredentialBinding);
    }
    Ok(())
}

fn topology_binding_matches_route(
    binding: &CredentialBindingV1,
    route: &NodeRoute,
) -> bool {
    binding.node_id.as_str() == route.node_id.as_str()
        && binding.node_incarnation.as_str().parse::<NodeIncarnationId>().ok()
            == Some(route.expected_incarnation_id)
}

pub fn apply_routed_observation_event(
    observation: &mut ObservationService,
    routed: RoutedNodeEvent,
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    observation.apply_ingress(routed_event_to_ingress(routed, received_at_ms)?)?;
    Ok(())
}

fn routed_event_to_ingress(
    routed: RoutedNodeEvent,
    received_at_ms: u64,
) -> Result<ObservationIngressEnvelope, HarnessRuntimeError> {
    if received_at_ms == 0 {
        return Err(HarnessRuntimeError::ZeroReceiveTime);
    }
    let RoutedNodeEvent { node_id, cursor, event } = routed;
    let payload = match event {
        C2NodeEvent::Observation { address, observation } => ObservationIngressPayload::Observation {
            address: ObservationTarget::Runtime { key: RuntimeSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                workspace_id: address.workspace_id,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            } },
            observation,
        },
        C2NodeEvent::ManagedObservation { record_id, observation } => {
            ObservationIngressPayload::Observation {
                address: ObservationTarget::Managed { key: ManagedSessionKey {
                    node_id: node_id.clone(),
                    incarnation_id: cursor.incarnation_id,
                    record_id,
                } },
                observation,
            }
        }
        C2NodeEvent::HarnessMcpReadCall { .. } => {
            return Err(HarnessRuntimeError::InvalidHarnessMcpEvent);
        }
        C2NodeEvent::SessionRecordUpserted { record } => {
            let runtime = record.active_session.map(|address| RuntimeSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                workspace_id: address.workspace_id,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            });
            ObservationIngressPayload::ManagedRecordUpserted { link: ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: node_id.clone(),
                    incarnation_id: cursor.incarnation_id,
                    record_id: record.record_id,
                },
                runtime,
            } }
        }
        C2NodeEvent::SessionRecordRemoved { record_id } => {
            ObservationIngressPayload::ManagedRecordRemoved { key: ManagedSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                record_id,
            } }
        }
        C2NodeEvent::ResyncRequired { oldest_available_sequence } => {
            ObservationIngressPayload::ResyncRequired { oldest: NodeCursor {
                incarnation_id: cursor.incarnation_id,
                sequence: oldest_available_sequence,
            } }
        }
        C2NodeEvent::Control { .. }
        | C2NodeEvent::TerminalFrame { .. }
        // Content, not an observation -- the same reason `TerminalFrame`
        // sits here. This path feeds the observation store, whose vocabulary
        // is semantic telemetry with correlation ids; an agent's text and
        // thinking carry none of that and have their own subscription. The
        // event still advances the cursor, so ignoring its payload never
        // opens a gap in the sequence.
        | C2NodeEvent::AgentStream { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceAdded { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::ManagedWorktreeUpserted { .. }
        | C2NodeEvent::ManagedWorktreeRemoved { .. } => ObservationIngressPayload::CursorOnly,
    };
    Ok(ObservationIngressEnvelope {
        node_id,
        cursor,
        received_at_ms,
        transport: ObservationTransport::C2,
        payload,
    })
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(1).max(1)
}

#[derive(Debug, Error)]
pub enum HarnessRuntimeError {
    #[error("observation receive timestamp must be nonzero")]
    ZeroReceiveTime,
    #[error(transparent)]
    C2(#[from] HarnessC2Error),
    #[error(transparent)]
    Observation(#[from] ObservationServiceError),
    #[error(transparent)]
    Harness(#[from] HarnessServiceError),
    #[error("Harness launch catalog is invalid")]
    LaunchCatalog,
    #[error("Harness dispatch preparation failed: {0}")]
    DispatchPreparation(&'static str),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("read host must bind exact IPv4 loopback")]
    NonLoopbackBind,
    #[error("read host bind failed")]
    BindFailed,
    #[error("read host accept failed")]
    AcceptFailed,
    #[error("read host stopped")]
    HostStopped,
    #[error("read host request frame is invalid")]
    InvalidFrame,
    #[error("read host request exceeds its bound")]
    RequestTooLarge,
    #[error("read host response exceeds its bound")]
    ResponseTooLarge,
    #[error("read host response is invalid")]
    InvalidReply,
    #[error("read host read failed")]
    ReadFailed,
    #[error("read host write failed")]
    WriteFailed,
    #[error("read host request deadline elapsed")]
    Deadline,
    #[error("credential binding is not current in observation authority")]
    CredentialBinding,
    #[error("operator credential digest failed")]
    OperatorCredentialDigest,
    #[error("durable services could not be flushed")]
    FlushFailed,
    #[error("transient harness MCP event reached observation persistence")]
    InvalidHarnessMcpEvent,
    #[error("harness MCP durable authority is unavailable")]
    HarnessMcpAuthority,
    #[error("bounded harness MCP capacity-rejection queue is full or closed")]
    HarnessMcpRejectQueueFull,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::{
        C2ControlEvent, C2ControlEventKind, C2ManagedSessionRecord,
        C2NodeEventEnvelope, C2NodeSnapshot, C2ObservationSupport,
        C2SessionSnapshot, C2SessionStatus, C2WorkspaceSnapshot,
    };
    use gate4agent_node_protocol::{
        ContextPackLineageReceipt, ManagedSessionState, NodeCursor, OpaqueHostPath,
        ProviderRuntimeStatuses, ResolvedContextPackReceipt, SessionMode, SessionRecordId,
        SpawnContextDigest,
    };
    use gate4agent_observation_protocol::{
        BlockAuthorityV1, ObservationEvidenceV1, ObservationKindV1, ObservationV1,
    };
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};
    use gate4agent_harness_protocol::{
        HarnessContextPackLineageV1, HarnessContinuationCleanupStateV1,
        HarnessContinuationRef, HarnessContinuationStateV1, HarnessContinuationV1,
        HarnessCreateTaskRequestV1, HarnessDeliveryBundleDigestV1,
        HarnessDeliveryBundleIdV1, HarnessDeliveryBundleRevisionV1,
        HarnessDeliveryBundleV1, HarnessDeliveryManifestDigestV2,
        HarnessDeliveryRef, HarnessDeliveryStateV1, HarnessDeliveryV1,
        HarnessExecutionModeV1, HarnessIdempotencyRef,
        HarnessOperationId, HarnessOperatorAuthorityV1, HarnessRequestDigest,
        HarnessReceiptRef, HarnessResolvedContextPackReceiptV1,
        HarnessRevision, HarnessRunId, HarnessRunIntentV1, HarnessRunV1,
        HarnessRunLifecycleV1, HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
        HarnessTaskV1,
        SessionGrantId, SessionGrantStateV1,
    };
    use gate4agent_harness_engine::{
        HarnessEngine, HarnessEngineCheckpointV1, HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
    };
    use gate4agent_types::{
        AgentId, AgentInstanceId, ProviderActivity, PtyScreenState, SessionGeneration,
        TerminalSize, TransportKind,
    };
    use gate4agent_c2_protocol::{SlimNodeInventory, SlimSession, SlimSessionStatus, SlimWorkspace};

    fn database_path() -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-harness-production-bridge-{}-{nonce}.sqlite",
            std::process::id(),
        ))
    }

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn operator_create_request() -> HarnessCreateTaskRequestV1 {
        HarnessCreateTaskRequestV1 {
            authority: HarnessOperatorAuthorityV1 {
                operation_id: HarnessOperationId::new(format!(
                    "hop_{}",
                    "a".repeat(24),
                )).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}",
                    "a".repeat(24),
                )).unwrap(),
                actor_id: selector("operator"),
                now_unix_ms: 10,
            },
            task_id: HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap(),
            title: "Operator task".to_owned(),
            body: "Bounded operator wire".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        }
    }

    fn task_start_dispatch_intent() -> HarnessDispatchIntentV1 {
        HarnessDispatchIntentV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap(),
            task_revision: HarnessRevision::new(2).unwrap(),
            run_id: HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(1).unwrap(),
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                "d".repeat(24),
            )).unwrap(),
            operation_revision: HarnessRevision::new(1).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "d".repeat(24),
            )).unwrap(),
            parent_run_id: None,
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: gate4agent_harness_protocol::HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("codex-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
        }
    }

    fn accepted_transition_plan(
        has_delivery: bool,
        has_continuation: bool,
        harness_mcp: crate::dispatch::HarnessMcpPolicyV1,
    ) -> crate::dispatch::HarnessLaunchPlanV1 {
        let privileged = has_delivery
            || has_continuation
            || harness_mcp == crate::dispatch::HarnessMcpPolicyV1::GrantBound;
        let plan = crate::dispatch::HarnessLaunchPlanV1 {
            plan_id: selector("accepted-transition"),
            revision: HarnessRevision::new(1).unwrap(),
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            worktree: gate4agent_harness_protocol::HarnessWorktreeIntentV1::Existing,
            provider_profile: selector("codex-default"),
            provider: AgentId::new("codex").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            terminal_size: TerminalSize {
                rows: 40,
                columns: 120,
            },
            prompt_source: crate::dispatch::HarnessPromptSourceV1::TaskBody,
            delivery: has_delivery.then(|| crate::dispatch::HarnessDeliveryPolicyV1 {
                selector: selector("skills"),
                bundle_id: SpawnBundleId::new("skills-bundle").unwrap(),
            }),
            continuation: if has_continuation {
                crate::dispatch::HarnessContinuationPolicyV1::ParentRun
            } else {
                crate::dispatch::HarnessContinuationPolicyV1::None
            },
            grant: if privileged {
                crate::dispatch::HarnessGrantPolicyV1::Exact {
                    grant_id: SessionGrantId::new(format!(
                        "hgrant_{}",
                        "e".repeat(24),
                    )).unwrap(),
                    revision: HarnessRevision::new(1).unwrap(),
                }
            } else {
                crate::dispatch::HarnessGrantPolicyV1::Operator
            },
            harness_mcp,
            deadline_ms: 30_000,
        };
        plan.validate().unwrap();
        plan
    }

    #[test]
    fn issued_operator_transfer_authority_selects_accepted_transition() {
        let ordinary = accepted_transition_plan(
            false,
            false,
            crate::dispatch::HarnessMcpPolicyV1::Disabled,
        );

        for (has_delivery, has_continuation, expected) in [
            (true, false, AcceptedSpawnTransition::Delivery),
            (false, true, AcceptedSpawnTransition::Continuation),
            (true, true, AcceptedSpawnTransition::DeliveryAndContinuation),
        ] {
            assert_eq!(
                accepted_spawn_transition(
                    &ordinary,
                    true,
                    has_delivery,
                    has_continuation,
                ).unwrap(),
                expected,
            );
        }
    }

    #[test]
    fn legacy_accepted_transition_paths_are_unchanged_and_mismatches_fail_closed() {
        let cases = [
            (
                false,
                false,
                crate::dispatch::HarnessMcpPolicyV1::Disabled,
                AcceptedSpawnTransition::Plain,
            ),
            (
                true,
                false,
                crate::dispatch::HarnessMcpPolicyV1::Disabled,
                AcceptedSpawnTransition::Delivery,
            ),
            (
                false,
                true,
                crate::dispatch::HarnessMcpPolicyV1::Disabled,
                AcceptedSpawnTransition::Continuation,
            ),
            (
                true,
                true,
                crate::dispatch::HarnessMcpPolicyV1::Disabled,
                AcceptedSpawnTransition::DeliveryAndContinuation,
            ),
            (
                false,
                false,
                crate::dispatch::HarnessMcpPolicyV1::GrantBound,
                AcceptedSpawnTransition::HarnessMcp,
            ),
            (
                true,
                false,
                crate::dispatch::HarnessMcpPolicyV1::GrantBound,
                AcceptedSpawnTransition::HarnessMcpDelivery,
            ),
            (
                false,
                true,
                crate::dispatch::HarnessMcpPolicyV1::GrantBound,
                AcceptedSpawnTransition::HarnessMcpContinuation,
            ),
            (
                true,
                true,
                crate::dispatch::HarnessMcpPolicyV1::GrantBound,
                AcceptedSpawnTransition::HarnessMcpDeliveryAndContinuation,
            ),
        ];
        for (has_delivery, has_continuation, harness_mcp, expected) in cases {
            let plan = accepted_transition_plan(
                has_delivery,
                has_continuation,
                harness_mcp,
            );
            assert_eq!(
                accepted_spawn_transition(
                    &plan,
                    false,
                    has_delivery,
                    has_continuation,
                ).unwrap(),
                expected,
            );
        }

        let ordinary = accepted_transition_plan(
            false,
            false,
            crate::dispatch::HarnessMcpPolicyV1::Disabled,
        );
        for (has_delivery, has_continuation) in [
            (false, false),
            (true, false),
            (false, true),
            (true, true),
        ] {
            let result = accepted_spawn_transition(
                &ordinary,
                false,
                has_delivery,
                has_continuation,
            );
            if !has_delivery && !has_continuation {
                assert_eq!(result.unwrap(), AcceptedSpawnTransition::Plain);
            } else {
                assert!(matches!(result, Err(HarnessRuntimeError::DispatchPreparation(_))));
            }
        }

        let legacy_delivery = accepted_transition_plan(
            true,
            false,
            crate::dispatch::HarnessMcpPolicyV1::Disabled,
        );
        assert!(matches!(
            accepted_spawn_transition(&legacy_delivery, false, false, false),
            Err(HarnessRuntimeError::DispatchPreparation(_)),
        ));
        assert!(matches!(
            accepted_spawn_transition(&legacy_delivery, false, true, true),
            Err(HarnessRuntimeError::DispatchPreparation(_)),
        ));
        let mut legacy_exact = accepted_transition_plan(
            false,
            false,
            crate::dispatch::HarnessMcpPolicyV1::Disabled,
        );
        legacy_exact.grant = crate::dispatch::HarnessGrantPolicyV1::Exact {
            grant_id: SessionGrantId::new(format!(
                "hgrant_{}",
                "f".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
        };
        legacy_exact.validate().unwrap();
        assert!(matches!(
            accepted_spawn_transition(&legacy_exact, false, true, false),
            Err(HarnessRuntimeError::DispatchPreparation(_)),
        ));
    }

    fn resolve_grant_target() -> HarnessGrantTargetV1 {
        HarnessGrantTargetV1 {
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            provider_profile: selector("claude-default"),
            mode: HarnessExecutionModeV1::Pty,
        }
    }

    /// Slice A(i): a harness-MCP dispatch with no exact grant to bind to
    /// (`HarnessGrantPolicyV1::Operator`) used to refuse outright
    /// (`begin_run_dispatch_with_harness_mcp`'s caller in
    /// `start_harness_host_with_operator_and_catalogs` returned
    /// `DispatchPreparation("harness mcp grant policy is Operator, not an
    /// Exact grant")`, unreachable from any test -- see
    /// `deterministic_default_grant_ids`'s own doc comment and the arc plan's
    /// O1). `resolve_harness_mcp_grant` is the extracted decision point: it
    /// now mints the run's default grant through `HarnessService::apply`
    /// instead of refusing, and the mint is real (visible on the engine,
    /// validated, revision 1, replay-safe) rather than a name-only stand-in.
    #[test]
    fn resolve_harness_mcp_grant_mints_default_grant_when_policy_is_operator() {
        let engine = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        );
        let mut harness = HarnessService::from_engine_for_test(engine);
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let dispatch_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "b".repeat(24),
        )).unwrap();

        let (grant_id, grant_revision) = resolve_harness_mcp_grant(
            &mut harness,
            &dispatch_operation_id,
            &crate::dispatch::HarnessGrantPolicyV1::Operator,
            &run_id,
            resolve_grant_target(),
            20,
        ).unwrap();
        assert_eq!(grant_revision, HarnessRevision::new(1).unwrap());

        let grant = harness.engine().grant(&grant_id)
            .expect("resolve_harness_mcp_grant applied a CreateGrant mutation")
            .clone();
        assert_eq!(grant.actor_run_id, run_id);
        assert_eq!(grant.state, SessionGrantStateV1::Active);
        assert_eq!(
            grant.read_permissions,
            gate4agent_harness_protocol::HarnessReadPermissionsV1 {
                tasks: gate4agent_harness_protocol::HarnessEntityReadScopeV1::SelfOnly,
                runs: gate4agent_harness_protocol::HarnessEntityReadScopeV1::SelfOnly,
                operations: gate4agent_harness_protocol::HarnessEntityReadScopeV1::SelfOnly,
            },
        );
        assert_eq!(
            grant.monitoring_visibility,
            gate4agent_harness_protocol::HarnessMonitoringVisibilityV1::Timeline,
        );
        assert!(!grant.task_permissions.create);
        assert!(!grant.task_permissions.mutate);
        assert!(!grant.context_permissions.export);
        assert!(!grant.context_permissions.restore);
        assert_eq!(grant.maximum_child_count, 0);
        assert_eq!(grant.maximum_child_depth, 0);
        assert!(grant.allowed_delivery_bundles.is_empty());

        let mut tool_ids = crate::read::allowed_tool_ids(&grant);
        tool_ids.sort();
        let mut expected: Vec<String> = gate4agent_harness_api::HARNESS_READ_TOOL_IDS
            .iter().map(|id| (*id).to_owned()).collect();
        expected.sort();
        assert_eq!(tool_ids, expected, "no mailbox tools exist yet, so this is the full ceiling");

        // Retrying the identical dispatch replays the same grant rather than
        // minting a second one -- required for a dispatch retry not to leak
        // an unbounded number of grants per run.
        let (replayed_grant_id, replayed_revision) = resolve_harness_mcp_grant(
            &mut harness,
            &dispatch_operation_id,
            &crate::dispatch::HarnessGrantPolicyV1::Operator,
            &run_id,
            resolve_grant_target(),
            20,
        ).unwrap();
        assert_eq!(replayed_grant_id, grant_id);
        assert_eq!(replayed_revision, grant_revision);
    }

    /// The `Exact` branch is the pre-existing, unchanged behaviour: a launch
    /// plan that already names a grant is used exactly as given, and no
    /// mutation is applied to the engine -- this is "a dispatch without
    /// harness MCP [grant minting]" staying unchanged.
    #[test]
    fn resolve_harness_mcp_grant_passes_through_an_exact_grant_untouched() {
        let engine = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        );
        let before = engine.checkpoint();
        let mut harness = HarnessService::from_engine_for_test(engine);
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let dispatch_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "b".repeat(24),
        )).unwrap();
        let exact_grant_id = SessionGrantId::new(format!("hgrant_{}", "e".repeat(24))).unwrap();
        let exact_revision = HarnessRevision::new(3).unwrap();

        let (grant_id, grant_revision) = resolve_harness_mcp_grant(
            &mut harness,
            &dispatch_operation_id,
            &crate::dispatch::HarnessGrantPolicyV1::Exact {
                grant_id: exact_grant_id.clone(),
                revision: exact_revision,
            },
            &run_id,
            resolve_grant_target(),
            20,
        ).unwrap();

        assert_eq!(grant_id, exact_grant_id);
        assert_eq!(grant_revision, exact_revision);
        assert_eq!(harness.engine().checkpoint(), before);
    }

    #[test]
    fn issued_transfer_ids_equal_legacy_transfer_ids() {
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "e".repeat(24),
        )).unwrap();
        let legacy = accepted_transition_plan(
            true,
            true,
            crate::dispatch::HarnessMcpPolicyV1::Disabled,
        );

        assert_eq!(
            deterministic_issued_dispatch_ids(&operation_id, true, true).unwrap(),
            deterministic_dispatch_ids(&operation_id, &legacy).unwrap(),
        );
    }

    #[test]
    fn task_started_replay_reply_does_not_schedule_or_start_dispatch_again() {
        let dispatch = task_start_dispatch_intent();
        let applied = HarnessOperatorResponseV1::TaskStarted(
            gate4agent_harness_protocol::HarnessTaskStartOutcomeV1 {
                dispatch: dispatch.clone(),
                replayed: false,
            },
        );
        let replay = HarnessOperatorResponseV1::TaskStarted(
            gate4agent_harness_protocol::HarnessTaskStartOutcomeV1 {
                dispatch: dispatch.clone(),
                replayed: true,
            },
        );
        let schedule_next = HarnessOperatorResponseV1::Schedule(
            gate4agent_harness_protocol::HarnessScheduleOutcomeV1::Dispatch(
                dispatch.clone(),
            ),
        );

        assert_eq!(
            scheduled_dispatch_from_operator_response(&applied),
            Some(dispatch.clone()),
        );
        assert_eq!(scheduled_dispatch_from_operator_response(&replay), None);
        assert_eq!(
            scheduled_dispatch_from_operator_response(&schedule_next),
            Some(dispatch),
        );
    }

    fn operator_create_intent(
        body: &str,
        submitted_at_unix_ms: u64,
    ) -> gate4agent_harness_api::HarnessOperatorIntentV1 {
        gate4agent_harness_api::HarnessOperatorIntentV1 {
            request_ref: gate4agent_harness_api::HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "7".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms,
            action: gate4agent_harness_api::HarnessOperatorActionV1::CreateTask {
                title: "Harness-owned task".to_owned(),
                body: body.to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Backlog,
            },
        }
    }

    fn running_harness_fixture() -> (
        HarnessService,
        HarnessTaskId,
        HarnessRunId,
        NodeRoute,
    ) {
        let task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let create_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "b".repeat(24),
        )).unwrap();
        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let actor = HarnessActorV1::User { actor_id: selector("operator") };
        let task = HarnessTaskV1 {
            task_id: task_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Lifecycle task".to_owned(),
            body: "Exact live control event".to_owned(),
            creator: actor.clone(),
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![run_id.clone()],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
        };
        let run = HarnessRunV1 {
            run_id: run_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id.clone(),
            operation_id: create_operation_id.clone(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("profile-a"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: Some(HarnessSessionBindingV1 {
                node_id: selector("node-a"),
                node_incarnation: selector(&incarnation.to_string()),
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-a"),
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id: 7,
                        generation: 3,
                    }),
                },
            }),
            lifecycle: HarnessRunLifecycleV1::Running,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
        };
        let create_operation = HarnessOperationV1 {
            operation_id: create_operation_id,
            revision: HarnessRevision::new(1).unwrap(),
            actor,
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "b".repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
            dispatched_at_unix_ms: Some(4),
            finished_at_unix_ms: Some(5),
        };
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![task],
            runs: vec![run],
            grants: Vec::new(),
            operations: vec![create_operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();
        let harness = HarnessService::from_engine_for_test(engine);
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: incarnation,
        };
        (harness, task_id, run_id, route)
    }

    fn exported_context_pack_receipt(route: &NodeRoute) -> ResolvedContextPackReceipt {
        ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-a").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: route.node_id.clone(),
                source_session: bound_session_address(),
                // Matches `running_harness_fixture()`'s own
                // `intent.provider_profile` -- `HarnessRunV1::validate()`
                // requires a run's `context_pack.lineage.source_provider` to
                // equal its own `intent.provider_profile`.
                source_provider: AgentId::new("profile-a").unwrap(),
            },
            source_message_count: 2,
            retained_message_count: 2,
            byte_len: 64,
            truncated: false,
        }
    }

    fn session_record_upserted(
        route: &NodeRoute,
        sequence: u64,
        record: C2ManagedSessionRecord,
    ) -> RoutedNodeEvent {
        RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor { incarnation_id: route.expected_incarnation_id, sequence },
            event: C2NodeEvent::SessionRecordUpserted { record },
        }
    }

    #[test]
    fn apply_live_context_pack_receipt_lands_within_one_live_event() {
        // Steady-state proof for `apply_live_context_pack_receipt`: exercises
        // the exact call `apply_or_buffer_host_live_event` makes right
        // alongside `apply_exact_control_lifecycle` on a healthy route,
        // reacting to a single live `SessionRecordUpserted` event -- no
        // snapshot, no resync, no reconnect anywhere in this test.
        let (mut harness, _task_id, run_id, route) = running_harness_fixture();
        assert!(harness.engine().run(&run_id).unwrap().context_pack.is_none());

        let receipt = exported_context_pack_receipt(&route);
        let record = C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-a").unwrap(),
            display_name: "Live export fixture".to_owned(),
            provider: AgentId::new("profile-a").unwrap(),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: Some(receipt.clone()),
            task_binding: None,
            provider_identity_present: true,
            created_at_unix_ms: 5,
            updated_at_unix_ms: 6,
        };
        let routed = session_record_upserted(&route, 6, record.clone());

        apply_live_context_pack_receipt(&mut harness, &route, &routed, 10).unwrap();
        let context_pack = harness.engine().run(&run_id).unwrap().context_pack.clone()
            .expect("run's context_pack did not land from the live SessionRecordUpserted event");
        assert_eq!(context_pack.digest, receipt.digest.as_str());
        assert_eq!(context_pack.lineage.source_provider.as_str(), "profile-a");
        let revision_after_first = harness.engine().run(&run_id).unwrap().revision;

        // Same (run_id, digest) idempotency the resync path relies on via
        // `record_run_context_pack`: a redundant live upsert of the same
        // already-exported record must not re-mutate the run.
        apply_live_context_pack_receipt(&mut harness, &route, &routed, 11).unwrap();
        assert_eq!(harness.engine().run(&run_id).unwrap().revision, revision_after_first);

        // A live event for an unrelated record must never touch this run.
        let mut other_record = record;
        other_record.record_id = SessionRecordId::new("record-b").unwrap();
        let other_routed = session_record_upserted(&route, 7, other_record);
        apply_live_context_pack_receipt(&mut harness, &route, &other_routed, 12).unwrap();
        assert_eq!(harness.engine().run(&run_id).unwrap().revision, revision_after_first);
    }

    #[test]
    fn reverse_attribution_reopens_with_exact_incarnation_and_rejects_false_attribution() {
        let (harness, task_id, run_id, route) = running_harness_fixture();
        let workspace = gate4agent_harness_api::HarnessReverseAttributionWorkspaceV1 {
            node_id: selector(route.node_id.as_str()),
            node_incarnation_id: HarnessNodeIncarnationV1::new(
                route.expected_incarnation_id.to_string(),
            ).unwrap(),
            workspace_id: selector("workspace-a"),
        };
        let reopened = HarnessService::from_engine_for_test(
            HarnessEngine::restore(harness.engine().checkpoint()).unwrap(),
        );

        for current in [&harness, &reopened] {
            let managed = project_reverse_attribution(
                current,
                HarnessReverseAttributionSubjectV1::ManagedRecord {
                    workspace: workspace.clone(),
                    record_id: selector("record-a"),
                },
            ).unwrap();
            assert_eq!(managed.outcome, HarnessReverseAttributionOutcomeV1::Attributed);
            assert_eq!(managed.links.len(), 1);
            assert_eq!(managed.links[0].task_id, task_id);
            assert_eq!(managed.links[0].run_id, run_id);
            assert_eq!(
                managed.links[0].relation,
                HarnessReverseAttributionRelationV1::ManagedRecordBinding,
            );
            assert_eq!(
                managed.links[0].binding,
                HarnessReverseAttributionBindingV1::ManagedRecord {
                    workspace: workspace.clone(),
                    record_id: selector("record-a"),
                    active_instance_id: Some(7),
                    active_generation: Some(3),
                },
            );

            let runtime = project_reverse_attribution(
                current,
                HarnessReverseAttributionSubjectV1::RuntimeSession {
                    workspace: workspace.clone(),
                    instance_id: 7,
                    generation: 3,
                },
            ).unwrap();
            assert_eq!(runtime.outcome, HarnessReverseAttributionOutcomeV1::Attributed);
            assert_eq!(runtime.links.len(), 1);
            assert_eq!(
                runtime.links[0].relation,
                HarnessReverseAttributionRelationV1::RuntimeSessionBinding,
            );

            let workspace_attribution = project_reverse_attribution(
                current,
                HarnessReverseAttributionSubjectV1::Workspace {
                    workspace: workspace.clone(),
                },
            ).unwrap();
            assert_eq!(
                workspace_attribution.links[0].relation,
                HarnessReverseAttributionRelationV1::WorkspaceBinding,
            );
            for subject in [
                HarnessReverseAttributionSubjectV1::FileScope {
                    workspace: workspace.clone(),
                    relative_path: gate4agent_harness_api::HarnessRepositoryPathV1::new(
                        "src/lib.rs",
                    ).unwrap(),
                },
                HarnessReverseAttributionSubjectV1::CommitScope {
                    workspace: workspace.clone(),
                    object_id: gate4agent_harness_api::HarnessGitObjectIdV1::new(
                        "a".repeat(40),
                    ).unwrap(),
                },
            ] {
                let scoped = project_reverse_attribution(current, subject).unwrap();
                assert_eq!(scoped.outcome, HarnessReverseAttributionOutcomeV1::Attributed);
                assert_eq!(scoped.links.len(), 1);
                assert_eq!(
                    scoped.links[0].relation,
                    HarnessReverseAttributionRelationV1::WorkspaceScope,
                );
            }
        }

        let wrong_incarnation = gate4agent_harness_api::HarnessReverseAttributionWorkspaceV1 {
            node_incarnation_id: HarnessNodeIncarnationV1::new(
                NodeIncarnationId::from_bytes([8; 16]).to_string(),
            ).unwrap(),
            ..workspace.clone()
        };
        for subject in [
            HarnessReverseAttributionSubjectV1::ManagedRecord {
                workspace: workspace.clone(),
                record_id: selector("record-b"),
            },
            HarnessReverseAttributionSubjectV1::RuntimeSession {
                workspace: workspace.clone(),
                instance_id: 7,
                generation: 4,
            },
            HarnessReverseAttributionSubjectV1::Workspace {
                workspace: wrong_incarnation,
            },
        ] {
            let unattributed = project_reverse_attribution(&reopened, subject).unwrap();
            assert_eq!(
                unattributed.outcome,
                HarnessReverseAttributionOutcomeV1::Unattributed,
            );
            assert!(unattributed.links.is_empty());
        }
    }

    fn lifecycle_event(
        sequence: u64,
        kind: C2ControlEventKind,
    ) -> C2NodeEventEnvelope {
        C2NodeEventEnvelope {
            sequence,
            event: C2NodeEvent::Control {
                address: SessionAddress {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    session: gate4agent_node_protocol::SessionKey {
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(3),
                    },
                },
                event: C2ControlEvent {
                    protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
                    sequence,
                    command_id: None,
                    instance_id: gate4agent_types::AgentInstanceId(7),
                    generation: gate4agent_types::SessionGeneration(3),
                    event: kind,
                },
            },
        }
    }

    fn bound_snapshot(
        node_id: &NodeId,
        record_state: ManagedSessionState,
        record_active_session: Option<SessionAddress>,
        status: C2SessionStatus,
    ) -> C2NodeSnapshot {
        C2NodeSnapshot {
                node_id: node_id.clone(),
                enabled_providers: vec![AgentId::new("kimi").unwrap()],
                provider_runtime_statuses: ProviderRuntimeStatuses::default(),
                workspaces: vec![C2WorkspaceSnapshot {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    canonical_root: OpaqueHostPath::utf8(
                        r"C:\fixture\workspace-a".to_owned(),
                    ).unwrap(),
                    sessions: vec![C2SessionSnapshot {
                        instance_id: AgentInstanceId(7),
                        agent_id: AgentId::new("kimi").unwrap(),
                        transport: TransportKind::Pty,
                        generation: SessionGeneration(3),
                        status,
                        pending_operation: None,
                        pending_input: None,
                        process_id: Some(700),
                        terminal_size: None,
                        terminal_frame: None,
                        provider_activity: ProviderActivity::Working,
                        provider_interaction_pending: false,
                        provider_identity_present: true,
                        screen_state: gate4agent_types::PtyScreenState::default(),
                    }],
                    worktree_service_mode: None,
                    managed_worktree_profiles: None,
                }],
                session_records: vec![C2ManagedSessionRecord {
                    record_id: SessionRecordId::new("record-a").unwrap(),
                    display_name: "Bound recovery fixture".to_owned(),
                    provider: AgentId::new("kimi").unwrap(),
                    mode: SessionMode::Pty,
                    state: record_state,
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    active_session: record_active_session,
                    environment_profile: None,
                    bundle: None,
                    context_id: None,
                    context: None,
                    exported_context: None,
                    task_binding: None,
                    provider_identity_present: true,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 9,
                }],
                agent_progress: Vec::new(),
                managed_worktrees: Vec::new(),
                launch_inventory: None,
                observation_support: Some(C2ObservationSupport {
                    events: true,
                    managed_target: true,
                    workflow_detail: false,
                }),
        }
    }

    fn bound_session_address() -> SessionAddress {
        SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        }
    }

    fn correlation_inventory(
        route: NodeRoute,
        snapshot: C2NodeSnapshot,
        observed_at_unix_ms: u64,
    ) -> HarnessRuntimeInventoryCache {
        let resync = HarnessObservationResync::test_fixture(route, 5, snapshot);
        let mut cache = HarnessRuntimeInventoryCache::default();
        cache.refresh(&resync, observed_at_unix_ms);
        cache
    }

    fn sample_slim_node_inventory(screen_state: PtyScreenState) -> SlimNodeInventory {
        let session = SlimSession {
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(1),
            agent_id: "codex".to_owned(),
            transport: TransportKind::Pty,
            status: SlimSessionStatus::Running,
            process_id: Some(1234),
            terminal_size: None,
            operation_pending: false,
            input_pending: false,
            screen_state,
        };
        let workspace_id = gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap();
        let mut workspaces = std::collections::BTreeMap::new();
        workspaces.insert(workspace_id.clone(), SlimWorkspace {
            workspace_id: workspace_id.clone(),
            canonical_root: "workspace-a".to_owned(),
            canonical_root_truncated: false,
            sessions: vec![session],
            session_count: 1,
            sessions_truncated: false,
            worktree_service_mode: None,
            managed_worktree_profiles: None,
        });
        SlimNodeInventory {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            provider_runtime_statuses: ProviderRuntimeStatuses::default(),
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
            workspaces,
            workspace_count: 1,
            workspaces_truncated: false,
            session_count: 1,
            sessions_truncated: false,
            managed_sessions: Vec::new(),
            managed_session_count: 0,
            managed_sessions_truncated: false,
            managed_worktrees: Vec::new(),
            managed_worktree_count: 0,
            managed_worktrees_truncated: false,
            launch_inventory: None,
        }
    }

    fn sample_runtime_node_inventory(screen_state: PtyScreenState) -> HarnessRuntimeNodeInventoryV1 {
        HarnessRuntimeNodeInventoryV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            observed_at_unix_ms: 100,
            event_sequence: 5,
            inventory: redact_runtime_inventory(sample_slim_node_inventory(screen_state)),
        }
    }

    /// The inventory's `screen_state` must track a terminal frame, not wait
    /// for the next observation resync.
    ///
    /// Measured against the live stack before this existed: a freshly
    /// spawned session's inventory entry read `unknown` for over twenty
    /// seconds while the node itself already said `ready`, and it only
    /// corrected when an unrelated resize forced a resync. Every other
    /// field the projection carries changes only when a session's lifecycle
    /// does, so the resync cadence suits them; a screen does not, and an
    /// inventory answering `Ready` for a pane that has since put up a
    /// vendor-update prompt is stale in the one direction a gate cannot
    /// tolerate.
    #[test]
    fn a_terminal_frame_refreshes_the_cached_screen_state_without_a_resync() {
        let mut cache = HarnessRuntimeInventoryCache::default();
        let node_id = NodeId::new("node-a").unwrap();
        cache.nodes.insert(
            node_id.clone(),
            sample_runtime_node_inventory(PtyScreenState::Unknown),
        );
        let key = RuntimeSessionKey {
            node_id: node_id.clone(),
            // `sample_runtime_node_inventory` stamps its incarnation as the
            // hex string "1" x 32, which is these bytes.
            incarnation_id: NodeIncarnationId::from_bytes([0x11; 16]),
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(1),
        };

        let refreshed = cache
            .apply_screen_state(&key, &PtyScreenState::Ready)
            .expect("a changed classification refreshes the node projection");
        assert_eq!(
            refreshed.inventory.workspaces["workspace-a"].sessions[0].screen_state,
            Some(map_screen_state(&PtyScreenState::Ready)),
        );

        // A frame carries the CURRENT classification on every frame, while
        // the node only republishes a changed one. Without this comparison
        // the harness would emit a `RuntimeInventoryChanged` per frame --
        // roughly fifty a second per live session -- so the second identical
        // application must report no change at all.
        assert!(cache.apply_screen_state(&key, &PtyScreenState::Ready).is_none());

        // A session that is not this one is never touched, however similar
        // its address: a wrong generation is a different session's screen.
        let stale_generation = RuntimeSessionKey {
            generation: SessionGeneration(2),
            ..key.clone()
        };
        assert!(cache
            .apply_screen_state(&stale_generation, &PtyScreenState::Failing {
                reason: "crash".to_owned(),
            })
            .is_none());
        assert_eq!(
            cache.nodes[&node_id].inventory.workspaces["workspace-a"].sessions[0].screen_state,
            Some(map_screen_state(&PtyScreenState::Ready)),
        );
    }

    /// `sample_runtime_node_inventory`'s one session is `TransportKind::Pty`
    /// (see `sample_slim_node_inventory`), so a cache hit against its exact
    /// address must refuse by name; an address absent from the cache
    /// (never resynced, or simply a different session) must fail OPEN --
    /// see `prompt_session_pty_refusal`'s own doc comment for why a miss is
    /// the one outcome this check must never treat as a refusal.
    #[test]
    fn prompt_session_pty_refusal_names_a_confirmed_pty_target_and_fails_open_on_a_miss() {
        let mut cache = HarnessRuntimeInventoryCache::default();
        let node_id = NodeId::new("node-a").unwrap();
        cache.nodes.insert(
            node_id.clone(),
            sample_runtime_node_inventory(PtyScreenState::Ready),
        );

        let pty_session = HarnessRuntimeSessionAddressV1 {
            node_id: "node-a".to_owned(),
            // `sample_runtime_node_inventory` stamps its incarnation as the
            // hex string "1" x 32.
            incarnation_id: "1".repeat(32),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        };
        assert!(matches!(
            prompt_session_pty_refusal(&cache, &pty_session),
            Some(HarnessOperatorHostErrorV1::UnsupportedTransport {
                transport: HarnessRuntimeTransportV1::Pty,
                ..
            }),
        ));

        let unknown_session = HarnessRuntimeSessionAddressV1 {
            instance_id: 999,
            ..pty_session
        };
        assert!(prompt_session_pty_refusal(&cache, &unknown_session).is_none());
    }

    /// `redact_runtime_inventory` always populates `screen_state: Some(..)`,
    /// and nothing downstream strips it back out: this wire has exactly one
    /// accepted version (see `HARNESS_OPERATOR_WIRE_VERSION`), so a
    /// `RuntimeInventoryChanged` event carries the real classification key
    /// on every serialized session unconditionally.
    #[test]
    fn a_runtime_inventory_changed_event_always_carries_the_screen_state_key() {
        let node = sample_runtime_node_inventory(PtyScreenState::Ready);
        let event = HarnessOperatorEventV1::RuntimeInventoryChanged { sequence: 3, node };

        let value = serde_json::to_value(&event).unwrap();
        let session = &value["node"]["inventory"]["workspaces"]["workspace-a"]["sessions"][0];
        assert_eq!(
            session["screen_state"],
            serde_json::to_value(map_screen_state(&PtyScreenState::Ready)).unwrap(),
            "expected the real screen_state key in {session}",
        );
    }

    #[test]
    fn run_context_source_worker_is_bounded_and_rejected_failures_are_typed() {
        let mut workers = RunContextSourceWorkerRegistry::default();
        for _ in 0..RUN_CONTEXT_SOURCE_WORKERS_MAX {
            assert!(workers.try_start());
        }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());
        assert_eq!(
            map_run_context_source_error(HarnessC2Error::RunContextSourceDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_run_context_source_error(HarnessC2Error::RunContextSourceRejected {
                code: NodeFailureCode::BackendOperationFailed,
            }),
            HarnessOperatorHostErrorV1::Unavailable,
        );
        assert_eq!(
            map_run_context_source_error(HarnessC2Error::RunContextSourceRouteMismatch),
            HarnessOperatorHostErrorV1::Conflict,
        );
    }

    #[test]
    fn run_context_source_restart_churn_triggers_recovery_and_reopens_exact_history() {
        let (harness, _task_id, run_id, route) = running_harness_fixture();
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        let runtime_inventory = correlation_inventory(route.clone(), snapshot, 20);
        let mut support = ObservationSupportRegistry::default();
        support.replace(
            route.node_id.clone(),
            route.expected_incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: false,
            }),
        );
        let observation_path = database_path();
        let mut observation = ObservationService::open(&observation_path).unwrap();
        let managed_key = ManagedSessionKey {
            node_id: route.node_id.clone(),
            incarnation_id: route.expected_incarnation_id,
            record_id: SessionRecordId::new("record-a").unwrap(),
        };
        observation.apply_resync(ObservationResyncBatch {
            node_id: route.node_id.clone(),
            incarnation_id: route.expected_incarnation_id,
            requested_after: 0,
            high_watermark: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: 5,
            },
            oldest_available_sequence: 1,
            records: vec![ManagedRecordLink {
                managed: managed_key.clone(),
                runtime: Some(RuntimeSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                }),
            }],
            records_complete: true,
            gaps: Vec::new(),
            events: Vec::new(),
        }).unwrap();
        let run = harness.engine().run(&run_id).unwrap();
        assert!(matches!(
            prepare_run_context_source_observation(
                run,
                &observation,
                &ObservationSupportRegistry::default(),
                &runtime_inventory,
            ),
            Err(HarnessOperatorHostErrorV1::Unavailable),
        ));
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        assert!(!support.is_authoritative(
            &route.node_id,
            route.expected_incarnation_id,
        ));
        let restart_prepared = prepare_run_context_source_observation(
            run,
            &observation,
            &support,
            &runtime_inventory,
        ).unwrap().unwrap();
        assert_eq!(restart_prepared.observed_after_sequence(), 5);
        support.replace(
            route.node_id.clone(),
            route.expected_incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: false,
            }),
        );
        let prepared = prepare_run_context_source_observation(
            run,
            &observation,
            &support,
            &runtime_inventory,
        ).unwrap().unwrap();
        assert_eq!(prepared.observed_after_sequence(), 5);
        let deadline = Instant::from_std(
            prepared.started_at() + RUN_CONTEXT_SOURCE_TOTAL_BUDGET,
        );
        assert!(deadline > Instant::now() + Duration::from_secs(9));
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        let (reply, _receive) = oneshot::channel();
        let pending = PendingRunContextSourceReply {
            prepared,
            projection: RunContextSourceProjection::Aggregate {
                message_count: 7,
                completed_turn_count: Some(3),
                total_tokens: Some(700),
            },
            deadline,
            reply,
        };
        let mut recovery = ObservationRecoveryRegistry::default();
        ensure_run_context_source_recovery_if_pending(
            std::slice::from_ref(&pending),
            &mut recovery,
            &run_id,
            &route,
            5,
        );
        assert!(recovery.contains(&route));
        assert_eq!(
            evaluate_pending_run_context_source(
                &harness,
                &mut observation,
                &support,
                &runtime_inventory,
                &pending,
            ).unwrap(),
            None,
        );

        apply_routed_observation_event(
            &mut observation,
            RoutedNodeEvent {
                node_id: route.node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: route.expected_incarnation_id,
                    sequence: 6,
                },
                event: C2NodeEvent::ManagedObservation {
                    record_id: SessionRecordId::new("record-a").unwrap(),
                    observation: ObservationV1 {
                        source_sequence: 99,
                        observed_at_unix_ms: Some(1_230),
                        evidence: ObservationEvidenceV1::HistoryProjection,
                        kind: ObservationKindV1::HistorySnapshot {
                            message_count: 7,
                            message_count_exact: true,
                            completed_turn_count: Some(3),
                            total_tokens: Some(700),
                        },
                        truncated: false,
                    },
                },
            },
            1_234,
        ).unwrap();
        assert_eq!(
            evaluate_pending_run_context_source(
                &harness,
                &mut observation,
                &support,
                &runtime_inventory,
                &pending,
            ).unwrap(),
            None,
        );
        support.replace(
            route.node_id.clone(),
            route.expected_incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: false,
            }),
        );
        let observed = evaluate_pending_run_context_source(
            &harness,
            &mut observation,
            &support,
            &runtime_inventory,
            &pending,
        ).unwrap().unwrap();
        assert_eq!(observed.run_id, run_id);
        assert_eq!(observed.feature_state, FeatureObservationStateV1::Observed);
        assert_eq!(observed.message_count, 7);
        assert_eq!(observed.observed_at_unix_ms, Some(1_234));
        observed.validate().unwrap();

        observation.close().unwrap();
        let reopened = ObservationService::open(&observation_path).unwrap();
        let source = context_source_option(
            &harness,
            &reopened,
            &support,
            &runtime_inventory,
            harness.engine().run(&run_id).unwrap(),
        ).unwrap().unwrap();
        assert_eq!(source.message_count, 7);
        assert_eq!(source.completed_turn_count, Some(3));
        reopened.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn run_correlation_projects_exact_stored_active_binding_and_current_availability() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        let cache = correlation_inventory(route, snapshot, 20);
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let response = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &DeliveryCatalogV2::default(),
            &cache,
            &TerminalBufferRegistry::default(),
            HarnessOperatorRequestV1::RunCorrelationGet {
                run_id: run_id.clone(),
            },
        ).unwrap();
        let HarnessOperatorResponseV1::RunCorrelation(correlation) = response else {
            panic!("run correlation response expected");
        };
        assert_eq!(correlation.run_id, run_id);
        assert_eq!(correlation.task_id, task_id);
        assert_eq!(correlation.run_revision, HarnessRevision::new(1).unwrap());
        assert_eq!(correlation.node_id.as_str(), "node-a");
        assert_eq!(correlation.workspace_id.as_str(), "workspace-a");
        assert_eq!(correlation.provider_profile.as_str(), "profile-a");
        assert_eq!(
            correlation.availability,
            HarnessRunCorrelationAvailabilityV1::Available,
        );
        assert_eq!(correlation.observed_at_unix_ms, Some(20));
        assert_eq!(
            correlation.session,
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: selector("record-a"),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 7,
                    generation: 3,
                }),
            }),
        );
        correlation.validate().unwrap();
        observation.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn run_correlation_projects_identity_pending_exact_active_binding_as_available() {
        let (harness, _, run_id, route) = running_harness_fixture();
        let mut snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::IdentityPending,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        snapshot.session_records[0].provider_identity_present = false;
        let cache = correlation_inventory(route.clone(), snapshot, 23);
        let current = project_run_correlation(harness.engine().run(&run_id).unwrap(), &cache)
            .unwrap();
        assert_eq!(
            current.availability,
            HarnessRunCorrelationAvailabilityV1::Available,
        );
        assert_eq!(current.observed_at_unix_ms, Some(23));

        let mismatched_bindings = [
            None,
            Some(SessionAddress {
                workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-b")
                    .unwrap(),
                ..bound_session_address()
            }),
            Some(SessionAddress {
                session: gate4agent_node_protocol::SessionKey {
                    instance_id: AgentInstanceId(8),
                    generation: SessionGeneration(3),
                },
                ..bound_session_address()
            }),
            Some(SessionAddress {
                session: gate4agent_node_protocol::SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(4),
                },
                ..bound_session_address()
            }),
        ];
        for active_session in mismatched_bindings {
            let mut snapshot = bound_snapshot(
                &route.node_id,
                ManagedSessionState::IdentityPending,
                active_session,
                C2SessionStatus::Running,
            );
            snapshot.session_records[0].provider_identity_present = false;
            let cache = correlation_inventory(route.clone(), snapshot, 24);
            let current = project_run_correlation(
                harness.engine().run(&run_id).unwrap(),
                &cache,
            ).unwrap();
            assert_eq!(
                current.availability,
                HarnessRunCorrelationAvailabilityV1::Unavailable,
            );
        }
    }

    #[test]
    fn run_transfer_reads_only_durable_records_for_the_exact_run() {
        let (mut harness, _, run_id, _) = running_harness_fixture();
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let response = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &DeliveryCatalogV2::default(),
            &HarnessRuntimeInventoryCache::default(),
            &TerminalBufferRegistry::default(),
            HarnessOperatorRequestV1::RunTransferGet {
                run_id: run_id.clone(),
            },
        ).unwrap();
        let HarnessOperatorResponseV1::RunTransfer(transfer) = response else {
            panic!("run transfer response expected");
        };
        assert_eq!(transfer.run_id, run_id);
        assert_eq!(transfer.run_revision, HarnessRevision::new(1).unwrap());
        assert_eq!(transfer.delivery, None);
        assert_eq!(transfer.continuation, None);
        transfer.validate().unwrap();

        let missing = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &DeliveryCatalogV2::default(),
            &HarnessRuntimeInventoryCache::default(),
            &TerminalBufferRegistry::default(),
            HarnessOperatorRequestV1::RunTransferGet {
                run_id: HarnessRunId::new(format!("hrun_{}", "f".repeat(24))).unwrap(),
            },
        );
        assert_eq!(missing, Err(HarnessOperatorHostErrorV1::NotFound));
        observation.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn run_transfer_projects_only_bounded_delivery_and_context_receipt_facts() {
        let (harness, task_id, run_id, route) = running_harness_fixture();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "d".repeat(24))).unwrap();
        let operation_id = HarnessOperationId::new(format!("hop_{}", "e".repeat(24))).unwrap();
        let bundle = HarnessDeliveryBundleV1 {
            selector: selector("reviewed-skill-bundle"),
            bundle_id: HarnessDeliveryBundleIdV1::new("bundle-a").unwrap(),
            revision: HarnessDeliveryBundleRevisionV1::new("r7").unwrap(),
            digest: HarnessDeliveryBundleDigestV1::new(format!(
                "sha256:{}",
                "a".repeat(64),
            )).unwrap(),
            manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "b".repeat(64),
            )).unwrap(),
        };
        let delivery = HarnessDeliveryV1 {
            delivery_ref: HarnessDeliveryRef::new(format!(
                "hdelivery_{}",
                "d".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            authority: gate4agent_harness_protocol::HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: grant_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
            },
            task_id,
            run_id: run_id.clone(),
            operation_id: operation_id.clone(),
            bundle,
            state: HarnessDeliveryStateV1::Prepared,
            stage_receipt: None,
            receipt: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        };
        let source_run_id = HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap();
        let source_binding = HarnessSessionBindingV1 {
            node_id: selector(route.node_id.as_str()),
            node_incarnation: selector(&route.expected_incarnation_id.to_string()),
            workspace_id: selector("workspace-a"),
            session: HarnessSessionIdentityV1::Managed {
                record_id: selector("source-record"),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 9,
                    generation: 2,
                }),
            },
        };
        let continuation = HarnessContinuationV1 {
            continuation_ref: HarnessContinuationRef::new(format!(
                "hcontinuation_{}",
                "e".repeat(24),
            )).unwrap(),
            receipt_ref: HarnessReceiptRef::new(format!(
                "hreceipt_{}",
                "e".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(3).unwrap(),
            state: HarnessContinuationStateV1::Exported,
            authority: gate4agent_harness_protocol::HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id,
                revision: HarnessRevision::new(1).unwrap(),
            },
            source_run_id: source_run_id.clone(),
            target_run_id: run_id.clone(),
            operation_id,
            node_id: source_binding.node_id.clone(),
            node_incarnation: source_binding.node_incarnation.clone(),
            workspace_id: source_binding.workspace_id.clone(),
            source_provider: selector("claude"),
            source_binding,
            context: Some(HarnessResolvedContextPackReceiptV1 {
                id: selector("context-a"),
                digest: format!("sha256:{}", "c".repeat(64)),
                lineage: HarnessContextPackLineageV1 {
                    source_node_id: selector(route.node_id.as_str()),
                    source_workspace_id: selector("workspace-a"),
                    source_instance_id: 9,
                    source_generation: 2,
                    source_provider: selector("claude"),
                },
                source_message_count: 7,
                retained_message_count: 5,
                byte_len: 4096,
                truncated: true,
            }),
            target_binding: None,
            prepared_at_unix_ms: 10,
            exporting_at_unix_ms: Some(11),
            exported_at_unix_ms: Some(12),
            bound_at_unix_ms: None,
            expired_at_unix_ms: None,
            outcome_unknown_at_unix_ms: None,
            outcome_unknown_reason: None,
            cleanup_state: HarnessContinuationCleanupStateV1::Retained,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
        };
        let transfer = project_run_transfer(
            harness.engine().run(&run_id).unwrap(),
            Some(&delivery),
            Some(&continuation),
        ).unwrap();
        assert_eq!(transfer.delivery.as_ref().unwrap().selector.as_str(), "reviewed-skill-bundle");
        let context = transfer.continuation.as_ref().unwrap().context.as_ref().unwrap();
        assert_eq!(context.source_message_count, 7);
        assert_eq!(context.retained_message_count, 5);
        assert_eq!(context.byte_len, 4096);
        assert!(context.truncated);
        assert_eq!(transfer.continuation.as_ref().unwrap().source_run_id, source_run_id);
        let encoded = serde_json::to_string(&transfer).unwrap();
        for private in ["source-record", "private-prompt", "provider-session-id", "C:\\\\private"] {
            assert!(!encoded.contains(private), "private field leaked: {private}");
        }
        transfer.validate().unwrap();
    }

    #[test]
    fn run_correlation_preserves_dormant_and_inline_historical_identities() {
        let (harness, _, run_id, route) = running_harness_fixture();
        let mut dormant = harness.engine().run(&run_id).unwrap().clone();
        dormant.lifecycle = HarnessRunLifecycleV1::Completed;
        dormant.result_disposition = Some(HarnessResultDispositionV1::Detached);
        dormant.binding.as_mut().unwrap().session = HarnessSessionIdentityV1::Managed {
            record_id: selector("record-a"),
            active_session: None,
        };
        let dormant_snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Dormant,
            None,
            C2SessionStatus::Exited { exit_code: Some(0) },
        );
        let cache = correlation_inventory(route.clone(), dormant_snapshot, 21);
        let dormant_view = project_run_correlation(&dormant, &cache).unwrap();
        assert_eq!(
            dormant_view.availability,
            HarnessRunCorrelationAvailabilityV1::Dormant,
        );
        assert_eq!(
            dormant_view.session,
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: selector("record-a"),
                active_session: None,
            }),
        );

        let mut inline = dormant;
        inline.intent.mode = HarnessExecutionModeV1::Inline;
        let inline_ref = gate4agent_harness_protocol::HarnessInlineRef::new(format!(
            "hinline_{}",
            "c".repeat(24),
        )).unwrap();
        inline.binding.as_mut().unwrap().session = HarnessSessionIdentityV1::Inline {
            inline_ref: inline_ref.clone(),
        };
        let inline_view = project_run_correlation(&inline, &cache).unwrap();
        assert_eq!(
            inline_view.session,
            HarnessRunSessionViewV1::Inline(HarnessInlineRunSessionV1 { inline_ref }),
        );
        assert_eq!(
            inline_view.availability,
            HarnessRunCorrelationAvailabilityV1::Unavailable,
        );
        assert_eq!(inline_view.observed_at_unix_ms, Some(21));
    }

    #[test]
    fn run_correlation_reports_missing_and_replaced_inventory_without_rewriting_binding() {
        let (harness, _, run_id, route) = running_harness_fixture();
        let stored = harness.engine().run(&run_id).unwrap();
        let not_observed = project_run_correlation(
            stored,
            &HarnessRuntimeInventoryCache::default(),
        ).unwrap();
        assert_eq!(
            not_observed.availability,
            HarnessRunCorrelationAvailabilityV1::NotObserved,
        );
        assert_eq!(not_observed.observed_at_unix_ms, None);

        let replacement_route = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([8; 16]),
        };
        let replacement_snapshot = bound_snapshot(
            &replacement_route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        let cache = correlation_inventory(replacement_route, replacement_snapshot, 22);
        let stale = project_run_correlation(stored, &cache).unwrap();
        assert_eq!(
            stale.availability,
            HarnessRunCorrelationAvailabilityV1::StaleIncarnation,
        );
        assert_eq!(stale.observed_at_unix_ms, Some(22));
        assert_eq!(
            stale.node_incarnation_id.as_str(),
            route.expected_incarnation_id.to_string(),
        );
        assert_eq!(stale.provider_profile.as_str(), "profile-a");
    }

    #[test]
    fn run_correlation_fails_closed_for_missing_or_malformed_stored_binding() {
        let (harness, _, run_id, _) = running_harness_fixture();
        let mut missing = harness.engine().run(&run_id).unwrap().clone();
        missing.lifecycle = HarnessRunLifecycleV1::Requested;
        missing.binding = None;
        assert_eq!(
            project_run_correlation(&missing, &HarnessRuntimeInventoryCache::default()),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );

        let mut malformed = harness.engine().run(&run_id).unwrap().clone();
        malformed.binding.as_mut().unwrap().node_incarnation = selector("not-hex");
        assert_eq!(
            project_run_correlation(&malformed, &HarnessRuntimeInventoryCache::default()),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );
    }

    #[test]
    fn resync_replays_host_down_exit_success_to_completed_review() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        let events = [lifecycle_event(
            5,
            C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
        )];
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Completed,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Review,
        );
        assert_eq!(
            harness.engine().run(&run_id).unwrap().result_disposition,
            Some(HarnessResultDispositionV1::Succeeded),
        );
    }

    #[test]
    fn distinct_running_control_is_noop_but_running_to_waiting_still_commits() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        let before_running = harness.engine().checkpoint();
        let events = [lifecycle_event(5, C2ControlEventKind::Running)];

        apply_replayed_lifecycle_events(&mut harness, &route, None, &events, 10)
            .unwrap();

        assert_eq!(
            harness.engine().run(&run_id).unwrap().revision,
            HarnessRevision::new(1).unwrap(),
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().revision,
            HarnessRevision::new(1).unwrap(),
        );
        assert_eq!(harness.engine().checkpoint(), before_running);

        let before_waiting = harness.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(6), &[], 11)
            .unwrap();

        assert_eq!(
            harness.engine().run(&run_id).unwrap().revision,
            HarnessRevision::new(2).unwrap(),
        );
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().revision,
            HarnessRevision::new(2).unwrap(),
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Waiting,
        );
        let after_waiting = harness.engine().checkpoint();
        assert_ne!(after_waiting, before_waiting);
        assert_eq!(
            after_waiting.operations.len(),
            before_waiting.operations.len() + 1,
        );
    }

    #[test]
    fn resync_exit_duplicate_and_checkpoint_reopen_are_unchanged() {
        let (mut harness, _task_id, _run_id, route) = running_harness_fixture();
        let events = [lifecycle_event(
            5,
            C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
        )];
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        let after_first_replay = harness.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(harness.engine().checkpoint(), after_first_replay);

        let reopened_engine = HarnessEngine::restore(after_first_replay).unwrap();
        let mut reopened = HarnessService::from_engine_for_test(reopened_engine);
        let before_reopen_replay = reopened.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut reopened, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(reopened.engine().checkpoint(), before_reopen_replay);
    }

    #[test]
    fn resync_eviction_without_exact_terminal_freezes_running_as_waiting() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &[], 11)
            .unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Waiting,
        );
        let after_gap = harness.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &[], 11)
            .unwrap();
        assert_eq!(harness.engine().checkpoint(), after_gap);
    }

    #[test]
    fn fresh_observation_recovery_restores_exact_running_snapshot_after_gap() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        freeze_bound_route_waiting(&mut harness, &route, 9, 10).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        let path = database_path();
        let mut observation = ObservationService::open(&path).unwrap();
        assert_eq!(durable_cursor_for(&observation, &route), None);
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );

        apply_snapshot_lifecycle(
            &mut harness,
            &route,
            9,
            &snapshot,
            &[],
            11,
        ).unwrap();
        observation.apply_resync(ObservationResyncBatch {
            node_id: route.node_id.clone(),
            incarnation_id: route.expected_incarnation_id,
            requested_after: 0,
            high_watermark: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: 9,
            },
            oldest_available_sequence: 1,
            records: vec![ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    record_id: SessionRecordId::new("record-a").unwrap(),
                },
                runtime: Some(RuntimeSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                }),
            }],
            records_complete: true,
            gaps: Vec::new(),
            events: Vec::new(),
        }).unwrap();

        assert_eq!(durable_cursor_for(&observation, &route), Some(9));
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Running,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Running,
        );
        let reopened = HarnessEngine::restore(harness.engine().checkpoint()).unwrap();
        assert_eq!(
            reopened.run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Running,
        );
        assert_eq!(
            reopened.task(&task_id).unwrap().state,
            HarnessTaskStateV1::Running,
        );
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn snapshot_recovery_keeps_unproven_runtime_states_waiting() {
        let cases = [
            (
                ManagedSessionState::Live,
                Some(SessionAddress {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    session: gate4agent_node_protocol::SessionKey {
                        instance_id: AgentInstanceId(8),
                        generation: SessionGeneration(3),
                    },
                }),
                C2SessionStatus::Running,
            ),
            (ManagedSessionState::Live, None, C2SessionStatus::Running),
            (ManagedSessionState::Dormant, None, C2SessionStatus::Running),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: None },
            ),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: Some(0) },
            ),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: Some(7) },
            ),
        ];
        for (record_state, active_session, status) in cases {
            let (mut harness, task_id, run_id, route) = running_harness_fixture();
            freeze_bound_route_waiting(&mut harness, &route, 9, 10).unwrap();
            let snapshot = bound_snapshot(
                &route.node_id,
                record_state,
                active_session,
                status,
            );
            apply_snapshot_lifecycle(&mut harness, &route, 9, &snapshot, &[], 11).unwrap();
            assert_eq!(
                harness.engine().run(&run_id).unwrap().lifecycle,
                HarnessRunLifecycleV1::Waiting,
            );
            assert_eq!(
                harness.engine().task(&task_id).unwrap().state,
                HarnessTaskStateV1::Waiting,
            );
        }
    }

    #[test]
    fn snapshot_recovery_terminal_failure_reconciles_task_result_refs() {
        // The 4th `reconcile_task_result_refs` insertion (A3 design §5.1):
        // `apply_snapshot_lifecycle` is the resync/recovery-catchup path and
        // can newly set `result_disposition` on a run that skips straight to
        // `Failed` via a reconnect snapshot, without ever passing through
        // `apply_exact_control_lifecycle`'s own reactive reconcile call.
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        freeze_bound_route_waiting(&mut harness, &route, 9, 10).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        assert!(harness.engine().task(&task_id).unwrap().result_refs.is_empty());
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Failed,
        );
        apply_snapshot_lifecycle(&mut harness, &route, 9, &snapshot, &[], 11).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Failed,
        );
        assert_eq!(
            harness.engine().run(&run_id).unwrap().result_disposition,
            Some(HarnessResultDispositionV1::Failed),
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().result_refs,
            vec![HarnessResultRef::for_run(&run_id)],
        );
    }

    #[test]
    fn durable_revoked_cleanup_backoff_never_drops_pending_authority() {
        let mut cleanup = PendingHarnessMcpAbort {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id: HarnessMcpReservationId::new(format!(
                "hmcpres_{}",
                "a".repeat(24),
            )).unwrap(),
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "b".repeat(64),
            )).unwrap(),
            attempts: 0,
            retry_after_unix_ms: 0,
            attempt_id: None,
        };
        for attempt in 1..=12 {
            let now = cleanup.retry_after_unix_ms.max(1);
            defer_harness_mcp_abort(&mut cleanup, now);
            assert_eq!(cleanup.attempts, attempt);
            assert!(cleanup.retry_after_unix_ms > now);
            assert!(cleanup.retry_after_unix_ms - now <= HARNESS_MCP_ABORT_RETRY_MAX_MS);
        }
        assert_eq!(cleanup.reservation_id.as_str(), format!(
            "hmcpres_{}",
            "a".repeat(24),
        ));
    }

    fn apply_managed_link(
        service: &mut ObservationService,
        incarnation_id: NodeIncarnationId,
        sequence: u64,
        generation: u64,
    ) {
        service.apply_ingress(ObservationIngressEnvelope {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor { incarnation_id, sequence },
            received_at_ms: 10 + sequence,
            transport: ObservationTransport::C2,
            payload: ObservationIngressPayload::ManagedRecordUpserted {
                link: ManagedRecordLink {
                    managed: ManagedSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        record_id: SessionRecordId::new("record-a").unwrap(),
                    },
                    runtime: Some(RuntimeSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                            "workspace-a",
                        ).unwrap(),
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(generation),
                    }),
                },
            },
        }).unwrap();
    }

    #[test]
    fn production_bridge_commits_managed_observation_without_harness_authority() {
        let path = database_path();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation_id = NodeIncarnationId::from_bytes([3; 16]);
        let record_id = SessionRecordId::new("record-a").unwrap();
        let mut service = ObservationService::open(&path).unwrap();
        apply_routed_observation_event(
            &mut service,
            RoutedNodeEvent {
                node_id: node_id.clone(),
                cursor: NodeCursor { incarnation_id, sequence: 1 },
                event: C2NodeEvent::ManagedObservation {
                    record_id: record_id.clone(),
                    observation: ObservationV1 {
                        source_sequence: 1,
                        observed_at_unix_ms: Some(10),
                        evidence: ObservationEvidenceV1::NodeLifecycle,
                        kind: ObservationKindV1::Ready,
                        truncated: false,
                    },
                },
            },
            11,
        ).unwrap();
        let target = ObservationTarget::Managed {
            key: ManagedSessionKey { node_id, incarnation_id, record_id },
        };
        assert_eq!(service.projection(&target).unwrap().timeline.len(), 1);
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[tokio::test]
    async fn read_host_rejects_second_frame_after_newline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(b"{}\n{}\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (mut server, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_single_frame(&mut server).await,
            Err(HarnessRuntimeError::InvalidFrame),
        ));
        client.await.unwrap();
    }

    #[tokio::test]
    async fn read_host_rejects_oversized_frame_before_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(&vec![b'x'; HARNESS_READ_REQUEST_MAX_BYTES + 1]).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (mut server, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_single_frame(&mut server).await,
            Err(HarnessRuntimeError::RequestTooLarge),
        ));
        client.await.unwrap();
    }

    #[tokio::test]
    async fn read_host_slow_incomplete_frame_returns_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, _receiver) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let subscriber_connections = Arc::new(Semaphore::new(HOST_SUBSCRIBER_LIMIT));
        let terminal_subscriber_connections =
            Arc::new(Semaphore::new(HOST_TERMINAL_SUBSCRIBER_LIMIT));
        let agent_stream_subscriber_connections =
            Arc::new(Semaphore::new(HOST_AGENT_STREAM_SUBSCRIBER_LIMIT));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                handle_connection(
                    stream,
                    commands,
                    None,
                    permit,
                    subscriber_connections,
                    terminal_subscriber_connections,
                    agent_stream_subscriber_connections,
                ).await,
                Err(HarnessRuntimeError::Deadline),
            ));
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"{}\n").await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let reply: HarnessReadReplyV1 = serde_json::from_slice(
            response.strip_suffix(b"\n").unwrap(),
        ).unwrap();
        assert_eq!(
            reply,
            HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn operator_slow_incomplete_frame_returns_operator_deadline_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, _receiver) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let authority = HarnessOperatorCredentialAuthority::new(credential.clone()).unwrap();
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let subscriber_connections = Arc::new(Semaphore::new(HOST_SUBSCRIBER_LIMIT));
        let terminal_subscriber_connections =
            Arc::new(Semaphore::new(HOST_TERMINAL_SUBSCRIBER_LIMIT));
        let agent_stream_subscriber_connections =
            Arc::new(Semaphore::new(HOST_AGENT_STREAM_SUBSCRIBER_LIMIT));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                handle_connection(
                    stream,
                    commands,
                    Some(authority),
                    permit,
                    subscriber_connections,
                    terminal_subscriber_connections,
                    agent_stream_subscriber_connections,
                ).await,
                Err(HarnessRuntimeError::Deadline),
            ));
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(format!(
            "{{\"version\":1,\"credential\":\"{}\"",
            credential.expose(),
        ).as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let reply: HarnessOperatorReplyV1 = serde_json::from_slice(
            response.strip_suffix(b"\n").unwrap(),
        ).unwrap();
        assert_eq!(
            reply,
            HarnessOperatorReplyV1::Error {
                error: HarnessOperatorHostErrorV1::Deadline,
            },
        );
        server.await.unwrap();
    }

    /// Item 2's direct proof, part one: the pure batching contract itself --
    /// N queued events collapse into ONE buffer, still framed as N separate
    /// newline-delimited JSON lines in order, exactly the concatenation of
    /// encoding each one individually. This is the buffer
    /// `write_operator_agent_stream_events` issues exactly one
    /// `write_all`+`flush` for, so this test locks in "one write" at the
    /// type level rather than by racing a real socket's syscall count.
    #[test]
    fn encode_operator_agent_stream_event_batch_concatenates_every_frame_in_order() {
        let events: Vec<HarnessOperatorAgentEventV1> = (0..3u64)
            .map(|sequence| HarnessOperatorAgentEventV1::Ping { sequence })
            .collect();
        let mut expected = Vec::new();
        for sequence in 0..3u64 {
            expected.append(
                &mut encode_operator_agent_stream_event(HarnessOperatorAgentEventV1::Ping {
                    sequence,
                }).unwrap(),
            );
        }
        let batched = encode_operator_agent_stream_event_batch(events).unwrap();
        assert_eq!(batched, expected);
        assert_eq!(batched.iter().filter(|byte| **byte == b'\n').count(), 3);
    }

    /// Item 2's direct proof, part two: a burst well past one batch's worth
    /// (`AGENT_STREAM_FORWARD_BATCH_MAX`), all queued before the forwarder
    /// task ever runs, still drains completely and in order across however
    /// many batch iterations it takes, then closes cleanly once the sender
    /// side is gone -- the forwarder never gets stuck handling only the
    /// first batch.
    #[tokio::test]
    async fn forwarder_drains_a_pre_queued_burst_spanning_multiple_batches() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let total = AGENT_STREAM_FORWARD_BATCH_MAX * 2 + 5;
        for sequence in 0..total as u64 {
            sender.try_send(HarnessOperatorAgentEventV1::Ping { sequence }).unwrap();
        }
        drop(sender);
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_operator_agent_stream_subscription(stream, receiver, permit).await
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let lines: Vec<&[u8]> = response
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), total);
        for (index, line) in lines.iter().enumerate() {
            let event: HarnessOperatorAgentEventV1 = serde_json::from_slice(line).unwrap();
            assert!(matches!(
                event,
                HarnessOperatorAgentEventV1::Ping { sequence } if sequence == index as u64,
            ));
        }
        assert!(server.await.unwrap().is_ok());
    }

    #[test]
    fn operator_auth_is_digest_only_constant_time_and_separate_from_agent_read() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let changed = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "b".repeat(64),
        )).unwrap();
        let authority = HarnessOperatorCredentialAuthority::new(credential.clone()).unwrap();
        assert!(authority.verify(&credential).unwrap());
        assert!(!authority.verify(&changed).unwrap());
        assert!(HarnessOperatorCredential::parse(
            format!("g4ah2_aa.{}", "0".repeat(64)),
        ).is_err());
        assert_eq!(format!("{credential:?}"), "HarnessOperatorCredential([REDACTED])");
    }

    #[test]
    fn operator_wire_preserves_exact_mutation_replay_and_categorical_conflict() {
        let harness_path = database_path();
        let observation_path = database_path();
        let mut harness = HarnessService::open(&harness_path).unwrap();
        let observation = ObservationService::open(&observation_path).unwrap();
        let support = ObservationSupportRegistry::default();
        let launch_catalog = HarnessLaunchCatalog::default();
        let runtime_inventory = HarnessRuntimeInventoryCache::default();
        let terminal_buffers = TerminalBufferRegistry::default();
        let request = operator_create_request();
        let first = execute_operator_request(
            &mut harness,
            &observation,
            &support,
            &launch_catalog,
            &DeliveryCatalogV2::default(),
            &runtime_inventory,
            &terminal_buffers,
            HarnessOperatorRequestV1::CreateTask { request: request.clone() },
        ).unwrap();
        let replay = execute_operator_request(
            &mut harness,
            &observation,
            &support,
            &launch_catalog,
            &DeliveryCatalogV2::default(),
            &runtime_inventory,
            &terminal_buffers,
            HarnessOperatorRequestV1::CreateTask { request: request.clone() },
        ).unwrap();
        assert_eq!(
            first,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        assert_eq!(
            replay,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Replayed),
        );
        let mut changed = request;
        changed.body = "changed intent".to_owned();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &DeliveryCatalogV2::default(),
                &runtime_inventory,
                &terminal_buffers,
                HarnessOperatorRequestV1::CreateTask { request: changed },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        harness.close().unwrap();
        observation.close().unwrap();
        for path in [harness_path, observation_path] {
            for candidate in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
        }
    }

    #[test]
    fn operator_intent_replays_across_reopen_and_credential_rotation() {
        let harness_path = database_path();
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let support = ObservationSupportRegistry::default();
        let launch_catalog = HarnessLaunchCatalog::default();
        let runtime_inventory = HarnessRuntimeInventoryCache::default();
        let terminal_buffers = TerminalBufferRegistry::default();
        let intent = operator_create_intent("Stable typed intent", 10);

        let first_credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let rotated_credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "b".repeat(64),
        )).unwrap();
        let first_auth = HarnessOperatorCredentialAuthority::new(first_credential.clone()).unwrap();
        let rotated_auth = HarnessOperatorCredentialAuthority::new(rotated_credential.clone()).unwrap();
        assert!(first_auth.verify(&first_credential).unwrap());
        assert!(rotated_auth.verify(&rotated_credential).unwrap());
        assert!(!rotated_auth.verify(&first_credential).unwrap());

        let authorized_before_rotation = authorize_operator_intent(intent.clone()).unwrap();
        let expected_task_id = match &authorized_before_rotation {
            HarnessOperatorRequestV1::CreateTask { request } => request.task_id.clone(),
            _ => panic!("create intent must authorize as create task"),
        };
        assert_eq!(
            authorized_before_rotation,
            authorize_operator_intent(intent.clone()).unwrap(),
        );

        let mut harness = HarnessService::open(&harness_path).unwrap();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &DeliveryCatalogV2::default(),
                &runtime_inventory,
                &terminal_buffers,
                HarnessOperatorRequestV1::SubmitIntent { intent: intent.clone() },
            ).unwrap(),
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        assert!(harness.engine().task(&expected_task_id).is_some());
        harness.close().unwrap();

        let mut harness = HarnessService::open(&harness_path).unwrap();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &DeliveryCatalogV2::default(),
                &runtime_inventory,
                &terminal_buffers,
                HarnessOperatorRequestV1::SubmitIntent { intent: intent.clone() },
            ).unwrap(),
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Replayed),
        );
        let mut changed_payload = intent.clone();
        changed_payload.action = gate4agent_harness_api::HarnessOperatorActionV1::CreateTask {
            title: "Harness-owned task".to_owned(),
            body: "Changed typed intent".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        };
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &DeliveryCatalogV2::default(),
                &runtime_inventory,
                &terminal_buffers,
                HarnessOperatorRequestV1::SubmitIntent { intent: changed_payload },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        let mut changed_time = intent;
        changed_time.submitted_at_unix_ms += 1;
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &DeliveryCatalogV2::default(),
                &runtime_inventory,
                &terminal_buffers,
                HarnessOperatorRequestV1::SubmitIntent { intent: changed_time },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        harness.close().unwrap();
        observation.close().unwrap();
        for path in [harness_path, observation_path] {
            for candidate in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
        }
    }

    #[test]
    fn runtime_inventory_live_event_invalidates_and_requests_exact_resync_refresh() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let active_session = SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        };
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(active_session),
            C2SessionStatus::Running,
        );
        let resync = HarnessObservationResync::test_fixture(
            route.clone(),
            5,
            snapshot.clone(),
        );
        let mut cache = HarnessRuntimeInventoryCache::default();
        cache.refresh(&resync, 10);
        assert_eq!(cache.page(None, 1).nodes.len(), 1);

        let mut recovery = ObservationRecoveryRegistry::default();
        let event = C2NodeEvent::SessionRecordUpserted {
            record: snapshot.session_records[0].clone(),
        };
        assert!(invalidate_runtime_inventory_for_event(
            &mut recovery,
            &route,
            &event,
        ));
        // The node stays readable across the whole invalidate-to-resync
        // window: a scheduled resync is not evidence the node is gone, and
        // an operator polling `RuntimeInventoryList` in that window used to
        // be told it was.
        assert_eq!(cache.page(None, 1).nodes.len(), 1);
        assert!(recovery.contains(&route));

        let refreshed = HarnessObservationResync::test_fixture(route, 6, snapshot);
        cache.refresh(&refreshed, 11);
        let page = cache.page(None, 1);
        page.validate().unwrap();
        assert_eq!(page.nodes[0].event_sequence, 6);
        assert_eq!(page.nodes[0].inventory.managed_sessions.len(), 1);
        assert_eq!(page.nodes[0].inventory.session_count, 1);
    }

    /// Two `ActionBlocked` observations against the same managed session
    /// tally into `blocked_count: 2` with `last_blocked_at_ms` reading the
    /// LATER of the two `received_at_ms` values, in the actual
    /// `RuntimeInventoryList` reply shape (`fill_managed_session_blocked_stats`)
    /// -- not just in the observation engine's own projection. Proof for
    /// plan item 4 (`gate4agent-blocked-action-event-2026-09-02.md` §2).
    #[test]
    fn action_blocked_observations_tally_into_the_runtime_inventory_blocked_stats() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let active_session = SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        };
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(active_session),
            C2SessionStatus::Running,
        );
        let resync = HarnessObservationResync::test_fixture(route.clone(), 5, snapshot);
        let mut cache = HarnessRuntimeInventoryCache::default();
        cache.refresh(&resync, 10);

        let observation_path = database_path();
        let mut observation = ObservationService::open(&observation_path).unwrap();
        let record_id = SessionRecordId::new("record-a").unwrap();
        for (sequence, received_at_ms) in [(1, 1_000), (2, 2_000)] {
            apply_routed_observation_event(
                &mut observation,
                RoutedNodeEvent {
                    node_id: route.node_id.clone(),
                    cursor: NodeCursor {
                        incarnation_id: route.expected_incarnation_id,
                        sequence,
                    },
                    event: C2NodeEvent::ManagedObservation {
                        record_id: record_id.clone(),
                        observation: ObservationV1 {
                            source_sequence: sequence,
                            observed_at_unix_ms: Some(received_at_ms),
                            evidence: ObservationEvidenceV1::ManagedHook,
                            kind: ObservationKindV1::ActionBlocked {
                                correlation_id: None,
                                tool_class: "bash".to_owned(),
                                authority: BlockAuthorityV1::HarnessGate,
                                reason_kind: None,
                                reason: "rule=deny-write".to_owned(),
                                help: None,
                            },
                            truncated: false,
                        },
                    },
                },
                received_at_ms,
            ).unwrap();
        }

        let mut page = cache.page(None, 1);
        fill_managed_session_blocked_stats(&mut page, &observation);
        page.validate().unwrap();
        assert_eq!(page.nodes[0].inventory.managed_sessions.len(), 1);
        let record = &page.nodes[0].inventory.managed_sessions[0];
        assert_eq!(record.record_id, "record-a");
        assert_eq!(record.blocked_count, 2);
        assert_eq!(record.last_blocked_at_ms, Some(2_000));

        observation.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn recovery_registry_retries_unhealthy_current_routes_and_freezes_absent_routes() {
        let node_a = NodeId::new("node-a").unwrap();
        let node_b = NodeId::new("node-b").unwrap();
        let incarnation_a = NodeIncarnationId::from_bytes([1; 16]);
        let incarnation_b = NodeIncarnationId::from_bytes([2; 16]);
        let mut support = ObservationSupportRegistry::default();
        support.replace(node_a.clone(), incarnation_a, None);
        support.replace(node_b.clone(), incarnation_b, None);
        support.mark_unhealthy(&node_a, incarnation_a);
        support.reconcile_current_routes(&[NodeRoute {
            node_id: node_a.clone(),
            expected_incarnation_id: incarnation_a,
        }]);
        assert!(!support.is_authoritative(&node_a, incarnation_a));
        assert!(!support.is_authoritative(&node_b, incarnation_b));
        support.replace(node_a.clone(), incarnation_a, None);
        assert!(support.is_authoritative(&node_a, incarnation_a));
        assert!(!support.is_authoritative(&node_b, incarnation_b));
    }

    #[test]
    fn specialized_delivery_failure_classifies_transport_as_unknown_and_local_as_failed() {
        assert_eq!(
            delivery_pre_dispatch_result(&HarnessC2Error::DeliveryTransport(
                gate4agent_c2_client::C2ControlError::Closed,
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            delivery_pre_dispatch_result(&HarnessC2Error::UnknownNode(
                NodeId::new("node-a").unwrap(),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_stale_delivery_completion_terminalizes_without_stopping_host() {
        assert_eq!(
            delivery_stage_completion_result(&HarnessRuntimeError::Harness(
                HarnessServiceError::InvalidStagedDeliveryProof(
                    "staged delivery proof is not from the current authoritative Node route",
                ),
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            delivery_stage_completion_result(&HarnessRuntimeError::DispatchPreparation(
                "test fixture: unrelated terminal cause",
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_dispatch_start_failure_is_terminally_classified() {
        assert_eq!(
            dispatch_start_pre_dispatch_result(&HarnessRuntimeError::C2(
                HarnessC2Error::ContextExportTransport(
                    gate4agent_c2_client::C2ControlError::Closed,
                ),
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            dispatch_start_pre_dispatch_result(&HarnessRuntimeError::C2(
                HarnessC2Error::UnknownNode(NodeId::new("node-a").unwrap()),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_staged_restart_skips_restage_and_selects_preflight() {
        assert_eq!(
            delivery_needs_staging(
                gate4agent_harness_protocol::HarnessDeliveryStateV1::Prepared,
            ).unwrap(),
            true,
        );
        assert_eq!(
            delivery_needs_staging(
                gate4agent_harness_protocol::HarnessDeliveryStateV1::Staged,
            ).unwrap(),
            false,
        );
        assert!(delivery_needs_staging(
            gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed,
        ).is_err());
    }

    #[test]
    fn specialized_phase_completion_rejects_wrong_operation_or_phase() {
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "d".repeat(24),
        )).unwrap();
        let other = HarnessOperationId::new(format!(
            "hop_{}",
            "e".repeat(24),
        )).unwrap();
        let active = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Delivery,
            accepted_transition_plan(false, false, crate::dispatch::HarnessMcpPolicyV1::Disabled),
        );
        assert!(active.is(&operation_id, CoordinatorDispatchPhase::Delivery));
        assert!(!active.is(&other, CoordinatorDispatchPhase::Delivery));
        assert!(!active.is(&operation_id, CoordinatorDispatchPhase::Preflight));
    }

    #[test]
    fn specialized_phase_order_is_delivery_then_continuation_then_preflight() {
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "f".repeat(24),
        )).unwrap();
        let delivery = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Delivery,
            accepted_transition_plan(false, false, crate::dispatch::HarnessMcpPolicyV1::Disabled),
        );
        let continuation = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Continuation,
            accepted_transition_plan(false, false, crate::dispatch::HarnessMcpPolicyV1::Disabled),
        );
        let preflight = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Preflight,
            accepted_transition_plan(false, false, crate::dispatch::HarnessMcpPolicyV1::Disabled),
        );
        assert!(delivery.is(&operation_id, CoordinatorDispatchPhase::Delivery));
        assert!(continuation.is(
            &operation_id,
            CoordinatorDispatchPhase::Continuation,
        ));
        assert!(preflight.is(&operation_id, CoordinatorDispatchPhase::Preflight));
        assert!(!delivery.is(
            &operation_id,
            CoordinatorDispatchPhase::Continuation,
        ));
        assert!(!continuation.is(
            &operation_id,
            CoordinatorDispatchPhase::Preflight,
        ));
    }

    #[test]
    fn specialized_continuation_restart_cuts_never_reexport() {
        use gate4agent_harness_protocol::HarnessContinuationStateV1;

        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Prepared),
            ContinuationResumeAction::BeginExport,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Exporting),
            ContinuationResumeAction::RecoverOutcomeUnknown,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Exported),
            ContinuationResumeAction::Preflight,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::OutcomeUnknown),
            ContinuationResumeAction::FinishOutcomeUnknown,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Expired),
            ContinuationResumeAction::FinishFailed,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Bound),
            ContinuationResumeAction::Reject,
        );
    }

    #[test]
    fn specialized_preflight_definite_failure_is_pre_dispatch_failed() {
        assert_eq!(
            preflight_pre_dispatch_result(&HarnessC2Error::UnknownNode(
                NodeId::new("node-a").unwrap(),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_harness_mcp_start_failure_is_failed_not_unknown() {
        assert!(matches!(
            dispatching_start_error_result(&HarnessRuntimeError::C2(
                HarnessC2Error::HarnessMcpArmEnqueue(
                    gate4agent_c2_client::C2ControlError::QueueFull,
                ),
            )),
            CoordinatorSpawnResult::Failed,
        ));
        assert!(matches!(
            dispatching_start_error_result(&HarnessRuntimeError::C2(
                HarnessC2Error::SpawnEnqueue(
                    gate4agent_c2_client::C2ControlError::Closed,
                ),
            )),
            CoordinatorSpawnResult::Failed,
        ));
    }

    #[test]
    fn specialized_harness_mcp_arm_finish_separates_rejection_from_lost_reply() {
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpRejected {
                code: NodeFailureCode::HarnessMcpUnavailable,
            }),
            CoordinatorSpawnResult::Failed,
        ));
        // A protocol violation (unnegotiated capability, route mismatch,
        // ...) is certain and non-retryable, same as a node rejection --
        // not a genuine lost reply.
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpTransport(
                gate4agent_c2_client::C2ControlError::Protocol(
                    "harness MCP read proxy capability was not negotiated".to_owned(),
                ),
            )),
            CoordinatorSpawnResult::Failed,
        ));
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpTransport(
                gate4agent_c2_client::C2ControlError::Closed,
            )),
            CoordinatorSpawnResult::OutcomeUnknown(_),
        ));
        // A named connection loss stays OutcomeUnknown and its cause
        // survives into the reason text `apply_spawn_result` logs.
        match harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpTransport(
            gate4agent_c2_client::C2ControlError::ConnectionLost {
                reason: gate4agent_c2_client::C2ConnectionLossReason::PipeClosed,
            },
        )) {
            CoordinatorSpawnResult::OutcomeUnknown(Some(reason)) => {
                assert!(reason.contains("pipe closed"), "reason: {reason}");
            }
            CoordinatorSpawnResult::OutcomeUnknown(None) => {
                panic!("expected the underlying connection-loss reason to survive")
            }
            _ => panic!("expected OutcomeUnknown for a genuine connection loss"),
        }
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpCorrelationMismatch),
            CoordinatorSpawnResult::OutcomeUnknown(_),
        ));
    }

    fn mcp_reservation_id(byte: char) -> HarnessMcpReservationId {
        HarnessMcpReservationId::new(format!("hmcpres_{}", byte.to_string().repeat(24)))
            .unwrap()
    }

    fn mcp_call_id(byte: char) -> HarnessMcpCallId {
        HarnessMcpCallId::new(format!("hmcpcall_{}", byte.to_string().repeat(24)))
            .unwrap()
    }

    #[test]
    fn harness_mcp_worker_registry_rejects_stale_and_duplicate_completions() {
        let reservation_id = mcp_reservation_id('a');
        let call_id = mcp_call_id('b');
        let revision = HarnessRevision::new(3).unwrap();
        let mut workers = HarnessMcpWorkerRegistry::default();
        workers.activations.insert(
            reservation_id.clone(),
            ActiveHarnessMcpActivation {
                attempt_id: 7,
                expected_revision: revision,
                updated_at_unix_ms: 11,
                reply: None,
            },
        );
        workers.relays.insert(
            (reservation_id.clone(), call_id.clone()),
            ActiveHarnessMcpRelay { attempt_id: 9 },
        );
        assert!(workers.accepts_activation(&reservation_id, 7, revision));
        assert!(!workers.accepts_activation(&reservation_id, 8, revision));
        assert!(!workers.accepts_activation(
            &reservation_id,
            7,
            HarnessRevision::new(4).unwrap(),
        ));
        assert!(workers.accepts_relay(&reservation_id, &call_id, 9));
        assert!(!workers.accepts_relay(&reservation_id, &call_id, 10));
        workers.activations.remove(&reservation_id);
        workers.relays.remove(&(reservation_id.clone(), call_id.clone()));
        assert!(!workers.accepts_activation(&reservation_id, 7, revision));
        assert!(!workers.accepts_relay(&reservation_id, &call_id, 9));

        let mut abort = PendingHarnessMcpAbort {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id,
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "c".repeat(64),
            )).unwrap(),
            attempts: 0,
            retry_after_unix_ms: 0,
            attempt_id: Some(12),
        };
        assert!(abort.accepts_completion(12));
        assert!(!abort.accepts_completion(13));
        abort.attempt_id = None;
        assert!(!abort.accepts_completion(12));
    }

    #[test]
    fn harness_mcp_worker_registry_enforces_one_global_bounded_cap() {
        let mut workers = HarnessMcpWorkerRegistry::default();
        let pending = BTreeMap::new();
        for index in 0..HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX {
            let digit = char::from(b'0' + index as u8);
            workers.relays.insert(
                (mcp_reservation_id(digit), mcp_call_id(digit)),
                ActiveHarnessMcpRelay { attempt_id: index as u64 + 1 },
            );
        }
        assert_eq!(
            workers.in_flight(&pending),
            HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX,
        );
        assert!(!workers.has_capacity(&pending));
    }

    #[test]
    fn native_history_worker_cap_and_failure_mapping_are_typed() {
        let mut workers = NativeHistoryWorkerRegistry::default();
        for _ in 0..NATIVE_HISTORY_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());
        assert_eq!(
            map_native_history_error(HarnessC2Error::NativeHistoryEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::NodeOffline(
                gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            )),
            HarnessOperatorHostErrorV1::Unavailable,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::IncarnationChanged {
                node_id: gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            }),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::NativeHistoryDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
    }

    #[test]
    fn run_read_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = RunReadWorkerRegistry::default();
        for _ in 0..RUN_READ_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let request = HarnessOperatorRequestV1::InspectRunWorkspace {
            run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
        };
        assert_eq!(
            operator_response_deadline(&request),
            HOST_RUN_READ_RESPONSE_DEADLINE,
        );
        assert_eq!(HOST_RUN_READ_RESPONSE_DEADLINE, Duration::from_secs(12));
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRouteMismatch),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRejected {
                code: NodeFailureCode::RepositoryFileNotFound,
            }),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRejected {
                code: NodeFailureCode::ResponseTooLarge,
            }),
            HarnessOperatorHostErrorV1::TooLarge,
        );
    }

    /// Node-scoped sibling of `run_read_worker_cap_deadline_and_failure_mapping_are_typed`:
    /// the same worker-cap/deadline/failure-mapping contract, for the read
    /// family that routes from a node/workspace pair with no run in flight.
    #[test]
    fn node_workspace_read_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = NodeWorkspaceReadWorkerRegistry::default();
        for _ in 0..NODE_WORKSPACE_READ_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let request = HarnessOperatorRequestV1::InspectNodeWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
        };
        assert_eq!(
            operator_response_deadline(&request),
            HOST_RUN_READ_RESPONSE_DEADLINE,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeWorkspaceReadEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeWorkspaceReadDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeWorkspaceReadRouteMismatch),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeOffline(
                gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            )),
            HarnessOperatorHostErrorV1::Unavailable,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeWorkspaceReadRejected {
                code: NodeFailureCode::RepositoryFileNotFound,
            }),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_node_workspace_read_error(HarnessC2Error::NodeWorkspaceReadRejected {
                code: NodeFailureCode::ResponseTooLarge,
            }),
            HarnessOperatorHostErrorV1::TooLarge,
        );
    }

    /// Write/create sibling of
    /// `node_workspace_read_worker_cap_deadline_and_failure_mapping_are_typed`:
    /// the same worker-cap/deadline/failure-mapping contract, plus the
    /// write-only `NodeFailureCode`s a read never produces -- most notably
    /// `RepositoryFileRevisionConflict` (the stale-`expected_revision` CAS
    /// rejection), which must land on the typed `Conflict` host error, not
    /// `Internal`.
    #[test]
    fn node_workspace_write_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = NodeWorkspaceWriteWorkerRegistry::default();
        for _ in 0..NODE_WORKSPACE_WRITE_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let request = HarnessOperatorRequestV1::CreateNodeWorkspaceFile {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: gate4agent_harness_api::HarnessRepositoryPathV1::new("notes/created.txt").unwrap(),
        };
        assert_eq!(
            operator_response_deadline(&request),
            HOST_RUN_READ_RESPONSE_DEADLINE,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteRouteMismatch),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeOffline(
                gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            )),
            HarnessOperatorHostErrorV1::Unavailable,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteRejected {
                code: NodeFailureCode::RepositoryFileRevisionConflict,
            }),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteRejected {
                code: NodeFailureCode::RepositoryEntryAlreadyExists,
            }),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteRejected {
                code: NodeFailureCode::RepositoryParentNotFound,
            }),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_node_workspace_write_error(HarnessC2Error::NodeWorkspaceWriteRejected {
                code: NodeFailureCode::ResponseTooLarge,
            }),
            HarnessOperatorHostErrorV1::TooLarge,
        );
    }

    /// Sibling of `node_workspace_read_worker_cap_deadline_and_failure_mapping_are_typed`
    /// for the direct-spawn family: worker cap, outer response deadline, the
    /// `HarnessC2Error`/`NodeFailureCode` mappings, and the pure
    /// route+receipt-to-wire-address projection.
    #[test]
    fn session_spawn_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = SessionSpawnWorkerRegistry::default();
        for _ in 0..SESSION_SPAWN_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let request = HarnessOperatorRequestV1::SpawnSession {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: "claude".to_owned(),
            provider_profile: "claude-default".to_owned(),
            mode: HarnessExecutionModeV1::Pty,
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 40, columns: 120 },
            approval_level: None,
        };
        assert_eq!(operator_response_deadline(&request), HOST_SESSION_SPAWN_RESPONSE_DEADLINE);

        assert_eq!(
            map_session_spawn_error(HarnessC2Error::SpawnEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_session_spawn_error(HarnessC2Error::SpawnProfileUnavailable(
                gate4agent_node_protocol::SpawnProfileId::new("claude-default").unwrap(),
            )),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_session_spawn_error(HarnessC2Error::SessionSpawnCancelled),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_session_spawn_node_failure(
                NodeFailureCode::ControllerBusy,
                "claude",
                HarnessRuntimeTransportV1::Pty,
            ),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_session_spawn_node_failure(
                NodeFailureCode::UnknownWorkspace,
                "claude",
                HarnessRuntimeTransportV1::Pty,
            ),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_session_spawn_node_failure(
                NodeFailureCode::UnsupportedTransport,
                "claude",
                HarnessRuntimeTransportV1::Acp,
            ),
            HarnessOperatorHostErrorV1::UnsupportedTransport {
                agent: "claude".to_owned(),
                transport: HarnessRuntimeTransportV1::Acp,
            },
        );

        let route = NodeRoute {
            node_id: gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            expected_incarnation_id: "1".repeat(32).parse().unwrap(),
        };
        let session = SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(41),
                generation: gate4agent_types::SessionGeneration(3),
            },
        };
        let address = session_address_from_receipt(&route, &session);
        assert_eq!(address.node_id, "node-a");
        assert_eq!(address.workspace_id, "workspace-a");
        assert_eq!(address.instance_id, 41);
        assert_eq!(address.generation, 3);
        address.validate().unwrap();
    }

    /// Sibling of `session_spawn_worker_cap_deadline_and_failure_mapping_are_typed`
    /// for the three thin session-control verbs: worker cap, outer response
    /// deadline, the `HarnessC2Error`/`NodeFailureCode` mappings, and the
    /// wire-`kind`-tag-driven response projection (`session_control_response`).
    #[test]
    fn session_control_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = SessionControlWorkerRegistry::default();
        for _ in 0..SESSION_CONTROL_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let session = HarnessRuntimeSessionAddressV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 41,
            generation: 3,
        };
        let resize_request = HarnessOperatorRequestV1::ResizeSession {
            session: session.clone(),
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 40, columns: 120 },
        };
        assert_eq!(
            operator_response_deadline(&resize_request),
            HOST_SESSION_CONTROL_RESPONSE_DEADLINE,
        );

        assert_eq!(
            map_session_control_error(HarnessC2Error::SessionControlEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_session_control_error(HarnessC2Error::SessionControlCancelled),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_session_control_error(HarnessC2Error::SessionControlRejected {
                code: NodeFailureCode::WorkspaceBusy,
            }),
            HarnessOperatorHostErrorV1::Busy,
        );
        // A session that is not there is NotFound, not Internal. The node
        // reaps a session during its own `Stop`, so an operator's follow-up
        // `RemoveSession` legitimately arrives after the session is gone --
        // and used to be answered as a host fault.
        for absent in [NodeFailureCode::UnknownSession, NodeFailureCode::UnknownWorkspace] {
            assert_eq!(
                map_session_control_error(HarnessC2Error::SessionControlRejected {
                    code: absent.clone(),
                }),
                HarnessOperatorHostErrorV1::NotFound,
                "{absent:?} names a missing target, not a broken host",
            );
        }

        let input_identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::WriteSessionInput {
                session: session.clone(),
                text: "hi".to_owned(),
            },
        );
        assert_eq!(input_identity.node_id(), "node-a");
        assert_eq!(input_identity.session_id(), "41/3");
        assert!(matches!(
            session_control_response(&input_identity),
            HarnessOperatorResponseV1::SessionInputWritten,
        ));
        let prompt_identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::PromptSession {
                session: session.clone(),
                text: "please continue".to_owned(),
            },
        );
        assert_eq!(prompt_identity.node_id(), "node-a");
        assert_eq!(prompt_identity.session_id(), "41/3");
        assert!(matches!(
            session_control_response(&prompt_identity),
            HarnessOperatorResponseV1::SessionPrompted,
        ));
        let stop_identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::StopSession { session, force: true },
        );
        assert!(matches!(
            session_control_response(&stop_identity),
            HarnessOperatorResponseV1::SessionStopped,
        ));
    }

    #[test]
    fn run_read_completion_accepts_lifecycle_only_change_and_rejects_binding_change() {
        let (harness, _, run_id, _) = running_harness_fixture();
        let captured = harness.engine().run(&run_id).unwrap();
        let prepared = PreparedRunRead::from_operator_request(
            captured,
            HarnessOperatorRequestV1::InspectRunWorkspace {
                run_id: run_id.clone(),
            },
        ).unwrap();

        let mut lifecycle_only = captured.clone();
        lifecycle_only.revision = HarnessRevision::new(2).unwrap();
        lifecycle_only.lifecycle = HarnessRunLifecycleV1::Waiting;
        lifecycle_only.updated_at_unix_ms += 1;
        assert_eq!(
            validate_run_read_completion_origin(Some(&lifecycle_only), &prepared),
            Ok(()),
        );

        let mut changed_binding = lifecycle_only;
        changed_binding.binding.as_mut().unwrap().workspace_id = selector("workspace-b");
        assert_eq!(
            validate_run_read_completion_origin(Some(&changed_binding), &prepared),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        assert_eq!(
            validate_run_read_completion_origin(None, &prepared),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );
    }

    #[test]
    fn harness_mcp_worker_cap_saturation_enqueues_typed_rejection() {
        use gate4agent_node_protocol::{SessionKey, WorkspaceId};
        use gate4agent_types::{AgentInstanceId, SessionGeneration};

        let (rejects, mut reject_rx) = mpsc::channel(
            MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
        );
        let reservation_id = mcp_reservation_id('f');
        let call_id = mcp_call_id('e');
        let plan = HarnessMcpRelayPlan {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id: reservation_id.clone(),
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "a".repeat(64),
            )).unwrap(),
            record_id: SessionRecordId::new("record-a").unwrap(),
            session: SessionAddress {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
            call_id: call_id.clone(),
            deadline_unix_ms: u64::MAX,
            outcome: Ok(vec![1, 2, 3]),
        };
        enqueue_harness_mcp_capacity_rejection(&rejects, plan).unwrap();
        let rejected = reject_rx.try_recv().unwrap();
        assert_eq!(rejected.reservation_id, reservation_id);
        assert_eq!(rejected.call_id, call_id);
        assert_eq!(rejected.outcome, Err(HarnessMcpRejectReasonV1::Internal));
    }

    #[derive(Clone, Copy)]
    enum BlockedHarnessMcpWorkerKind { Activation, Abort, Relay }

    async fn assert_actor_inputs_responsive_with_blocked_mcp_worker(
        kind: BlockedHarnessMcpWorkerKind,
    ) {
        let reservation_id = mcp_reservation_id('d');
        let call_id = mcp_call_id('e');
        let (commands, mut command_rx) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let (events, mut event_rx) = mpsc::channel(1);
        let (release, blocked) = oneshot::channel::<()>();
        let worker_commands = commands.clone();
        let worker_reservation_id = reservation_id.clone();
        let worker_call_id = call_id.clone();
        let worker = tokio::spawn(async move {
            let _ = blocked.await;
            let command = match kind {
                BlockedHarnessMcpWorkerKind::Activation => {
                    HostCommand::HarnessMcpActivationFinished {
                        reservation_id: worker_reservation_id,
                        attempt_id: 1,
                        expected_revision: HarnessRevision::new(1).unwrap(),
                        result: Err(HarnessC2Error::TopologyClosed),
                    }
                }
                BlockedHarnessMcpWorkerKind::Abort => {
                    HostCommand::HarnessMcpAbortFinished {
                        reservation_id: worker_reservation_id,
                        attempt_id: 1,
                        result: Err(HarnessC2Error::TopologyClosed),
                    }
                }
                BlockedHarnessMcpWorkerKind::Relay => {
                    HostCommand::HarnessMcpRelayFinished {
                        reservation_id: worker_reservation_id,
                        call_id: worker_call_id,
                        attempt_id: 1,
                        result: Err(HarnessRuntimeError::HostStopped),
                    }
                }
            };
            worker_commands.send(command).await.unwrap();
        });
        assert!(timeout(Duration::from_millis(10), command_rx.recv()).await.is_err());
        let (reply, _receive) = oneshot::channel();
        commands.send(HostCommand::Operator {
            request: HarnessOperatorRequestV1::CreateTask {
                request: operator_create_request(),
            },
            reply,
            cancel: None,
        }).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_millis(50), command_rx.recv()).await.unwrap(),
            Some(HostCommand::Operator { .. }),
        ));
        events.send(7u8).await.unwrap();
        assert_eq!(
            timeout(Duration::from_millis(50), event_rx.recv()).await.unwrap(),
            Some(7),
        );
        release.send(()).unwrap();
        assert!(timeout(Duration::from_millis(50), command_rx.recv())
            .await.unwrap().is_some());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_activation_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Activation,
        ).await;
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_abort_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Abort,
        ).await;
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_relay_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Relay,
        ).await;
    }

    fn recovery_event(route: &NodeRoute, sequence: u64) -> RoutedNodeEvent {
        RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence,
            },
            event: C2NodeEvent::ResyncRequired {
                oldest_available_sequence: sequence,
            },
        }
    }

    #[test]
    fn recovery_buffer_overflow_clears_events_and_requires_immediate_followup() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let mut recovery = RouteObservationRecovery::new(route.clone());
        for sequence in 1..=OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX as u64 {
            recovery.buffer(recovery_event(&route, sequence));
        }
        assert_eq!(
            recovery.buffered.len(),
            OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX,
        );
        recovery.buffer(recovery_event(
            &route,
            OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX as u64 + 1,
        ));
        assert!(recovery.buffered.is_empty());
        assert_eq!(recovery.buffered_bytes, 0);
        assert!(recovery.overflowed);
        assert!(recovery.refresh_after_completion);
        recovery.prepare_follow_up();
        assert!(!recovery.overflowed);
        assert!(!recovery.refresh_after_completion);
        assert!(recovery.retry_after <= Instant::now());
    }

    #[test]
    fn recovery_stale_completion_is_rejected_by_attempt_route_and_cursor() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let changed = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([2; 16]),
        };
        let mut recovery = RouteObservationRecovery::new(route.clone());
        recovery.attempt = Some(ObservationRecoveryAttempt {
            attempt_id: 7,
            requested_after: 11,
        });
        assert!(recovery.accepts_completion(&route, 7, 11));
        assert!(!recovery.accepts_completion(&route, 8, 11));
        assert!(!recovery.accepts_completion(&route, 7, 12));
        assert!(!recovery.accepts_completion(&changed, 7, 11));
    }

    #[test]
    fn recovery_applies_lifecycle_before_cursor_and_buffer_drain() {
        let (mut harness, _task_id, run_id, route) = running_harness_fixture();
        let path = database_path();
        let mut observation = ObservationService::open(&path).unwrap();
        let mut recovery = RouteObservationRecovery::new(route.clone());
        recovery.buffer(RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: 6,
            },
            event: C2NodeEvent::ManagedObservation {
                record_id: SessionRecordId::new("record-a").unwrap(),
                observation: ObservationV1 {
                    source_sequence: 6,
                    observed_at_unix_ms: Some(12),
                    evidence: ObservationEvidenceV1::NodeLifecycle,
                    kind: ObservationKindV1::Ready,
                    truncated: false,
                },
            },
        });

        apply_replayed_lifecycle_events(
            &mut harness,
            &route,
            Some(4),
            &[lifecycle_event(
                5,
                C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
            )],
            11,
        ).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Completed,
        );
        assert_eq!(durable_cursor_for(&observation, &route), None);

        for (_, routed) in std::mem::take(&mut recovery.buffered) {
            apply_routed_observation_event(&mut observation, routed, 12).unwrap();
        }
        assert_eq!(durable_cursor_for(&observation, &route), Some(6));
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn recovery_topology_refresh_invalidates_old_route_and_refetches_same_route() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let replacement = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([2; 16]),
        };
        let mut registry = ObservationRecoveryRegistry::default();
        registry.ensure_route(route.clone()).attempt = Some(ObservationRecoveryAttempt {
            attempt_id: 1,
            requested_after: 3,
        });
        registry.reconcile_topology(&[route.clone()]);
        assert!(registry.routes.get(&ObservationRecoveryRegistry::key(&route))
            .unwrap().refresh_after_completion);

        registry.reconcile_topology(&[replacement.clone()]);
        assert!(!registry.contains(&route));
        assert!(registry.contains(&replacement));
        assert!(registry.routes.get(&ObservationRecoveryRegistry::key(&replacement))
            .unwrap().attempt.is_none());
    }

    #[tokio::test]
    async fn operator_command_remains_responsive_while_recovery_completion_is_blocked() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let (commands, mut receive) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let (release, blocked) = oneshot::channel::<()>();
        let worker_commands = commands.clone();
        let worker_route = route.clone();
        let worker = tokio::spawn(async move {
            let _ = blocked.await;
            worker_commands.send(HostCommand::ObservationRecoveryFinished {
                route: worker_route.clone(),
                attempt_id: 1,
                requested_after: 0,
                result: Err(HarnessC2Error::UnknownNode(worker_route.node_id)),
            }).await.unwrap();
        });
        let (reply, _reply_receive) = oneshot::channel();
        commands.send(HostCommand::Operator {
            request: HarnessOperatorRequestV1::CreateTask {
                request: operator_create_request(),
            },
            reply,
            cancel: None,
        }).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_millis(50), receive.recv()).await.unwrap(),
            Some(HostCommand::Operator { .. }),
        ));
        release.send(()).unwrap();
        worker.await.unwrap();
        assert!(matches!(
            receive.recv().await,
            Some(HostCommand::ObservationRecoveryFinished {
                route: completed_route,
                attempt_id: 1,
                requested_after: 0,
                ..
            }) if completed_route == route,
        ));
    }

    #[test]
    fn credential_mint_binding_requires_current_observation_generation() {
        let path = database_path();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation_id = NodeIncarnationId::from_bytes([4; 16]);
        let record_id = SessionRecordId::new("record-a").unwrap();
        let workspace_id = gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap();
        let mut service = ObservationService::open(&path).unwrap();
        service.apply_ingress(ObservationIngressEnvelope {
            node_id: node_id.clone(),
            cursor: NodeCursor { incarnation_id, sequence: 1 },
            received_at_ms: 10,
            transport: ObservationTransport::C2,
            payload: ObservationIngressPayload::ManagedRecordUpserted {
                link: ManagedRecordLink {
                    managed: ManagedSessionKey {
                        node_id,
                        incarnation_id,
                        record_id,
                    },
                    runtime: Some(RuntimeSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        workspace_id,
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(2),
                    }),
                },
            },
        }).unwrap();
        let mut binding = CredentialBindingV1 {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation: selector(&incarnation_id.to_string()),
            record_id: selector("record-a"),
            instance_id: 7,
            generation: 1,
        };
        let mut support = ObservationSupportRegistry::default();
        support.replace(
            NodeId::new("node-a").unwrap(),
            incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: true,
            }),
        );
        assert_eq!(
            verify_observation_credential_binding(
                &service,
                &support,
                &binding,
            ),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        binding.generation = 2;
        assert_eq!(
            verify_observation_credential_binding(&service, &support, &binding),
            Ok(()),
        );
        support.mark_unhealthy(&NodeId::new("node-a").unwrap(), incarnation_id);
        assert_eq!(
            verify_observation_credential_binding(&service, &support, &binding),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn credential_read_rejects_observation_generation_replaced_after_mint() {
        let path = database_path();
        let incarnation_id = NodeIncarnationId::from_bytes([4; 16]);
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_managed_link(&mut observation, incarnation_id, 1, 1);
        let mut support = ObservationSupportRegistry::default();
        support.replace(
            NodeId::new("node-a").unwrap(),
            incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: true,
            }),
        );
        let authority = CredentialAuthority::new().unwrap();
        let credential = authority.mint(
            harness.engine(),
            crate::credential::tests::binding(1, 1),
            100,
            200,
        ).unwrap();
        apply_managed_link(&mut observation, incarnation_id, 2, 2);
        assert_eq!(
            crate::read::verify_and_execute_read(
                &harness,
                &observation,
                &support,
                &authority,
                &credential,
                150,
                gate4agent_harness_api::HarnessReadRequestV1::ContextGet,
            ),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn topology_change_ready_before_command_denies_stale_binding() {
        let binding = crate::credential::tests::binding(1, 1);
        let changed_route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([5; 16]),
        };
        assert!(!topology_binding_matches_route(&binding, &changed_route));
        let current_route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([4; 16]),
        };
        assert!(topology_binding_matches_route(&binding, &current_route));
    }

    /// Pure-logic coverage of `SubscriberRegistry`'s overflow/recovery/
    /// pruning semantics, independent of any real connection: `emit` against
    /// a capacity-2 channel the test deliberately fills (`Full` ->
    /// `needs_baseline`, the overflowing event dropped for this subscriber
    /// only — and its sequence number burned, sequences are monotonic with
    /// gaps, never reused); `recover_lagged` landing exactly `Lagged` then
    /// `SnapshotBaseline` — a pair, so recovery needs TWO free slots and
    /// keeps the flag until a pass lands both; and a closed receiver
    /// pruning the subscriber outright on the next `emit`.
    #[test]
    fn subscriber_registry_full_then_lagged_recovery_and_closed_removal() {
        let identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::SubscribeEvents {},
        );
        let mut subscribers = SubscriberRegistry::default();
        let (sender, mut receiver) = mpsc::channel::<HarnessOperatorEventV1>(2);
        subscribers.insert(sender, identity);
        assert_eq!(subscribers.subscribers.len(), 1);

        // Two sends fill the capacity-2 queue.
        subscribers.emit(|sequence| HarnessOperatorEventV1::Lagged { sequence });
        subscribers.emit(|sequence| HarnessOperatorEventV1::Lagged { sequence });
        assert!(!subscribers.subscribers[0].needs_baseline);
        // Third send overflows the full queue: the event is dropped for
        // this subscriber (its sequence number burned) and it is marked
        // lagged, not removed.
        subscribers.emit(|sequence| HarnessOperatorEventV1::Lagged { sequence });
        assert!(subscribers.subscribers[0].needs_baseline);
        assert_eq!(subscribers.subscribers.len(), 1);

        // Recovery cannot land the Lagged+SnapshotBaseline pair while the
        // queue is still full — the flag must survive the failed pass.
        let harness_path = database_path();
        let harness = HarnessService::open(&harness_path).unwrap();
        let runtime_inventory = HarnessRuntimeInventoryCache::default();
        subscribers.recover_lagged(&harness, &runtime_inventory);
        assert!(subscribers.subscribers[0].needs_baseline);

        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorEventV1::Lagged { sequence: 0 },
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorEventV1::Lagged { sequence: 1 },
        ));
        assert!(receiver.try_recv().is_err(), "the overflowing send must not have queued");

        // With the queue drained, one pass lands Lagged then
        // SnapshotBaseline and clears the flag. Sequences 2 (overflow) and
        // 3 (failed recovery Lagged) were burned, never reused.
        subscribers.recover_lagged(&harness, &runtime_inventory);
        assert!(!subscribers.subscribers[0].needs_baseline);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorEventV1::Lagged { sequence: 4 },
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorEventV1::SnapshotBaseline { sequence: 5, .. },
        ));
        let _ = fs::remove_file(&harness_path);

        // A closed receiver is pruned outright on the next attempted send.
        drop(receiver);
        subscribers.emit(|sequence| HarnessOperatorEventV1::Lagged { sequence });
        assert!(subscribers.subscribers.is_empty());
    }

    /// The operator-subscriber slot leak's own fix, pinned directly: a
    /// subscriber whose peer already went away (receiver dropped, exactly
    /// what an abandoned `HarnessEventSubscription` looks like server-side)
    /// is reaped by `emit_subscriber_keepalive` the same way a real event's
    /// failed write would reap it -- no dependency on any task/run/inventory
    /// change ever happening.
    #[test]
    fn keepalive_tick_reaps_a_subscriber_whose_write_fails() {
        let identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::SubscribeEvents {},
        );
        let mut subscribers = SubscriberRegistry::default();
        let (sender, receiver) = mpsc::channel::<HarnessOperatorEventV1>(HOST_SUBSCRIBER_QUEUE_CAPACITY);
        subscribers.insert(sender, identity);
        assert_eq!(subscribers.subscribers.len(), 1);

        // Abandon the connection exactly as a dropped `HarnessEventSubscription`
        // does: only the receiver goes away, nothing else touches the registry.
        drop(receiver);

        emit_subscriber_keepalive(&mut subscribers);
        assert!(
            subscribers.subscribers.is_empty(),
            "a keep-alive write to a subscriber whose receiver is gone must reap it",
        );
    }

    /// Sibling of the above: the same tick must be a complete no-op for a
    /// subscriber that is still alive -- it neither drops it nor marks it
    /// `needs_baseline`, and the frame that lands is the `Ping` the tick
    /// actually sent.
    #[test]
    fn keepalive_tick_does_not_disturb_a_live_subscriber() {
        let identity = OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::SubscribeEvents {},
        );
        let mut subscribers = SubscriberRegistry::default();
        let (sender, mut receiver) = mpsc::channel::<HarnessOperatorEventV1>(HOST_SUBSCRIBER_QUEUE_CAPACITY);
        subscribers.insert(sender, identity);

        emit_subscriber_keepalive(&mut subscribers);

        assert_eq!(subscribers.subscribers.len(), 1, "a live subscriber must survive the tick");
        assert!(!subscribers.subscribers[0].needs_baseline);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorEventV1::Ping { sequence: 0 },
        ));
        assert!(receiver.try_recv().is_err(), "exactly one keep-alive frame per tick");
    }
}
