use crate::protocol::{
    HarnessMcpActivationDigest, HarnessMcpCallId, HarnessMcpLocalReplyV1,
    HarnessMcpLocalRequestV1, HarnessMcpLocalToken, HarnessMcpRejectReasonV1,
    HarnessMcpReplyChunkHexV1, HarnessMcpReservationId, HarnessReadHostErrorV1,
    NodeEvent, ResolvedSpawnReceipt, SessionAddress, SessionRecordId, SpawnSpec,
    read_json_frame_limited_body_timeout, write_json_frame_limited,
    MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES, MAX_HARNESS_MCP_CALL_DEADLINE_MS,
    MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES, MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
    MAX_HARNESS_MCP_PENDING_CALLS_PER_SESSION, MAX_HARNESS_MCP_RESERVATION_TTL_MS,
};
use gate4agent_node_wire::{random_nonce, LocalServerStream, OwnerOnlyLocalListener};
use gate4agent_types::{AgentId, AgentInstanceId};
use ring::digest::{Context as DigestContext, SHA256};
use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{oneshot, Notify};
use tokio::time::{timeout, Duration};

const MAX_RESERVATIONS: usize = 128;
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(3);

type EventSink = Arc<dyn Fn(NodeEvent) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct HarnessMcpProxyRegistry {
    inner: Arc<Mutex<RegistryState>>,
    helper_program: Arc<ReviewedHarnessMcpProgram>,
    event_sink: Arc<Mutex<Option<EventSink>>>,
}

struct RegistryState {
    reservations: BTreeMap<HarnessMcpReservationId, Reservation>,
    tombstones: BTreeMap<HarnessMcpReservationId, ReservationTombstone>,
    pending_calls: usize,
    local_connections: usize,
}

struct ReservationTombstone {
    activation_digest: HarnessMcpActivationDigest,
    expires_at_unix_ms: u64,
}

struct Reservation {
    activation_digest: HarnessMcpActivationDigest,
    spawn_spec: SpawnSpec,
    expires_at_unix_ms: u64,
    endpoint: PathBuf,
    token: HarnessMcpLocalToken,
    runtime: Arc<ReservationRuntime>,
    spawned: Option<SpawnBinding>,
    receipt: Option<ResolvedSpawnReceipt>,
    activation: Option<ActivationBinding>,
    aborted: bool,
    calls: BTreeMap<HarnessMcpCallId, PendingCall>,
    local_connections: usize,
}

struct ReservationRuntime {
    state: Mutex<RuntimeState>,
    changed: Notify,
}

struct RuntimeState {
    activation: Option<ActivationBinding>,
    revoked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SpawnBinding {
    instance_id: AgentInstanceId,
    provider: AgentId,
    session: SessionAddress,
    record_id: SessionRecordId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivationBinding {
    record_id: SessionRecordId,
    session: SessionAddress,
    provider_root_pid: u32,
}

struct PendingCall {
    binding: ActivationBinding,
    next_offset: u32,
    bytes: Vec<u8>,
    terminal: Option<oneshot::Sender<HarnessMcpLocalReplyV1>>,
}

pub(crate) struct PreparedHarnessMcpSpawn {
    pub(crate) reservation_id: HarnessMcpReservationId,
    pub(crate) activation_digest: HarnessMcpActivationDigest,
    pub(crate) provider: AgentId,
    endpoint: PathBuf,
    token: HarnessMcpLocalToken,
    helper_program: Arc<ReviewedHarnessMcpProgram>,
}

impl PreparedHarnessMcpSpawn {
    pub(crate) fn endpoint(&self) -> &Path { &self.endpoint }
    pub(crate) fn token(&self) -> &HarnessMcpLocalToken { &self.token }
    pub(crate) fn helper_program(&self) -> &Path { &self.helper_program.path }
    pub(crate) fn verify_helper_program(&self) -> Result<(), HarnessMcpProxyError> {
        self.helper_program.verify()
    }
}

#[derive(Clone)]
pub(crate) struct ReviewedHarnessMcpProgram {
    path: PathBuf,
    identity: HelperFileIdentity,
    byte_len: u64,
    modified_unix_nanos: u128,
    sha256: [u8; 32],
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct HelperFileIdentity {
    first: u64,
    second: u64,
}

impl ReviewedHarnessMcpProgram {
    pub(crate) fn review(path: PathBuf) -> Result<Self, HarnessMcpProxyError> {
        if !path.is_absolute() { return Err(HarnessMcpProxyError::Unavailable); }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| HarnessMcpProxyError::Unavailable)?;
        if !metadata.file_type().is_file() || metadata_is_reparse(&metadata) {
            return Err(HarnessMcpProxyError::Unavailable);
        }
        let modified_unix_nanos = metadata.modified()
            .map_err(|_| HarnessMcpProxyError::Unavailable)?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| HarnessMcpProxyError::Unavailable)?
            .as_nanos();
        let sha256 = hash_file(&path)?;
        Ok(Self {
            path,
            identity: helper_file_identity(&metadata),
            byte_len: metadata.len(),
            modified_unix_nanos,
            sha256,
        })
    }

    pub(crate) fn verify(&self) -> Result<(), HarnessMcpProxyError> {
        let current = Self::review(self.path.clone())?;
        if current.identity != self.identity
            || current.byte_len != self.byte_len
            || current.modified_unix_nanos != self.modified_unix_nanos
            || !constant_time_equal(&current.sha256, &self.sha256)
        {
            return Err(HarnessMcpProxyError::Unavailable);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum HarnessMcpProxyError {
    #[error("harness MCP reservation is unavailable")]
    Unavailable,
    #[error("harness MCP reservation was not found")]
    NotFound,
    #[error("harness MCP reservation conflicts with existing state")]
    Conflict,
    #[error("harness MCP reservation expired")]
    Expired,
    #[error("harness MCP binding does not match")]
    BindingMismatch,
    #[error("harness MCP reservation is not activated")]
    NotActivated,
    #[error("harness MCP call was not found")]
    CallNotFound,
    #[error("harness MCP reply chunk is out of order")]
    ChunkOutOfOrder,
    #[error("harness MCP reply is too large")]
    ResponseTooLarge,
}

impl HarnessMcpProxyRegistry {
    pub(crate) fn new(helper_program: ReviewedHarnessMcpProgram) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryState {
                reservations: BTreeMap::new(),
                tombstones: BTreeMap::new(),
                pending_calls: 0,
                local_connections: 0,
            })),
            helper_program: Arc::new(helper_program),
            event_sink: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn set_event_sink(&self, sink: EventSink) {
        *self.event_sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    pub(crate) async fn arm(
        &self,
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        spawn_spec: SpawnSpec,
        expires_at_unix_ms: u64,
    ) -> Result<u64, HarnessMcpProxyError> {
        let now = unix_time_ms();
        if expires_at_unix_ms <= now
            || expires_at_unix_ms.saturating_sub(now) > MAX_HARNESS_MCP_RESERVATION_TTL_MS
        {
            return Err(HarnessMcpProxyError::Expired);
        }
        {
            let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            prune_registry(&mut state, now);
            if state.tombstones.contains_key(&reservation_id) {
                return Err(HarnessMcpProxyError::Conflict);
            }
            if let Some(existing) = state.reservations.get(&reservation_id) {
                if existing.activation_digest == activation_digest
                    && existing.spawn_spec == spawn_spec
                    && existing.expires_at_unix_ms == expires_at_unix_ms
                    && !existing.aborted
                {
                    return Ok(existing.expires_at_unix_ms);
                }
                return Err(HarnessMcpProxyError::Conflict);
            }
            if state.reservations.len() >= MAX_RESERVATIONS {
                return Err(HarnessMcpProxyError::Unavailable);
            }
        }

        let token = new_token()?;
        let (endpoint, listener) = prebind_listener().await?;
        let runtime = Arc::new(ReservationRuntime {
            state: Mutex::new(RuntimeState { activation: None, revoked: false }),
            changed: Notify::new(),
        });
        {
            let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            prune_registry(&mut state, now);
            if state.tombstones.contains_key(&reservation_id) {
                return Err(HarnessMcpProxyError::Conflict);
            }
            if state.reservations.contains_key(&reservation_id) {
                return Err(HarnessMcpProxyError::Conflict);
            }
            if state.reservations.len() >= MAX_RESERVATIONS {
                return Err(HarnessMcpProxyError::Unavailable);
            }
            state.reservations.insert(reservation_id.clone(), Reservation {
                activation_digest,
                spawn_spec,
                expires_at_unix_ms,
                endpoint,
                token: token.clone(),
                runtime: Arc::clone(&runtime),
                spawned: None,
                receipt: None,
                activation: None,
                aborted: false,
                calls: BTreeMap::new(),
                local_connections: 0,
            });
        }
        let registry = self.clone();
        tokio::spawn(async move {
            registry.accept_loop(reservation_id, token, runtime, listener, expires_at_unix_ms).await;
        });
        Ok(expires_at_unix_ms)
    }

    pub(crate) fn prepare_spawn(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        spec: &SpawnSpec,
        deadline_unix_ms: u64,
        provider: AgentId,
    ) -> Result<PreparedHarnessMcpSpawn, HarnessMcpProxyError> {
        let now = unix_time_ms();
        if deadline_unix_ms <= now {
            return Err(HarnessMcpProxyError::Expired);
        }
        let state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, activation_digest, now)?;
        if reservation.spawn_spec != *spec || reservation.spawned.is_some() {
            return Err(HarnessMcpProxyError::Conflict);
        }
        self.helper_program.verify()?;
        Ok(PreparedHarnessMcpSpawn {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
            provider,
            endpoint: reservation.endpoint.clone(),
            token: reservation.token.clone(),
            helper_program: Arc::clone(&self.helper_program),
        })
    }

    pub(crate) fn replay_spawn(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        spec: &SpawnSpec,
    ) -> Result<Option<ResolvedSpawnReceipt>, HarnessMcpProxyError> {
        let state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, activation_digest, unix_time_ms())?;
        if reservation.spawn_spec != *spec {
            return Err(HarnessMcpProxyError::Conflict);
        }
        Ok(reservation.receipt.clone())
    }

    pub(crate) fn mark_spawned(
        &self,
        prepared: &PreparedHarnessMcpSpawn,
        session: SessionAddress,
        record_id: SessionRecordId,
        receipt: ResolvedSpawnReceipt,
    ) -> Result<(), HarnessMcpProxyError> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get_mut(&prepared.reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, &prepared.activation_digest, unix_time_ms())?;
        let binding = SpawnBinding {
            instance_id: session.session.instance_id,
            provider: prepared.provider.clone(),
            session,
            record_id,
        };
        match reservation.spawned.as_ref() {
            None => {
                reservation.spawned = Some(binding);
                reservation.receipt = Some(receipt);
            }
            Some(existing) if existing == &binding => {}
            Some(_) => return Err(HarnessMcpProxyError::Conflict),
        }
        Ok(())
    }

    pub(crate) fn activate(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        provider_root_pid: u32,
    ) -> Result<(), HarnessMcpProxyError> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get_mut(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, activation_digest, unix_time_ms())?;
        let spawned = reservation.spawned.as_ref().ok_or(HarnessMcpProxyError::BindingMismatch)?;
        if spawned.record_id != *record_id || spawned.session != *session || provider_root_pid == 0 {
            return Err(HarnessMcpProxyError::BindingMismatch);
        }
        let activation = ActivationBinding {
            record_id: record_id.clone(),
            session: session.clone(),
            provider_root_pid,
        };
        match reservation.activation.as_ref() {
            Some(existing) if existing != &activation => return Err(HarnessMcpProxyError::Conflict),
            Some(_) => return Ok(()),
            None => reservation.activation = Some(activation.clone()),
        }
        let mut runtime = reservation.runtime.state.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.activation = Some(activation);
        drop(runtime);
        reservation.runtime.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn abort(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
    ) -> Result<Option<AgentInstanceId>, HarnessMcpProxyError> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = unix_time_ms();
        prune_registry(&mut state, now);
        if let Some(tombstone) = state.tombstones.get(reservation_id) {
            return if tombstone.activation_digest == *activation_digest {
                Ok(None)
            } else {
                Err(HarnessMcpProxyError::Conflict)
            };
        }
        let mut reservation = state.reservations.remove(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        if reservation.activation_digest != *activation_digest {
            state.reservations.insert(reservation_id.clone(), reservation);
            return Err(HarnessMcpProxyError::Conflict);
        }
        let instance_id = reservation.spawned.as_ref().map(|spawned| spawned.instance_id);
        reservation.aborted = true;
        revoke_runtime(&reservation.runtime);
        state.pending_calls = state.pending_calls.saturating_sub(reservation.calls.len());
        state.local_connections = state.local_connections.saturating_sub(reservation.local_connections);
        state.tombstones.insert(reservation_id.clone(), ReservationTombstone {
            activation_digest: activation_digest.clone(),
            expires_at_unix_ms: now.saturating_add(MAX_HARNESS_MCP_RESERVATION_TTL_MS),
        });
        Ok(instance_id)
    }

    pub(crate) fn revoke_session(&self, session: &SessionAddress) -> Option<AgentInstanceId> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.values_mut().find(|reservation| {
            reservation.spawned.as_ref().is_some_and(|spawned| spawned.session == *session)
        })?;
        reservation.aborted = true;
        revoke_runtime(&reservation.runtime);
        let call_count = reservation.calls.len();
        reservation.calls.clear();
        state.pending_calls = state.pending_calls.saturating_sub(call_count);
        Some(session.session.instance_id)
    }

    pub(crate) fn shutdown(&self) -> Vec<AgentInstanceId> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut instances = Vec::new();
        for reservation in state.reservations.values_mut() {
            reservation.aborted = true;
            revoke_runtime(&reservation.runtime);
            if let Some(spawned) = reservation.spawned.as_ref() {
                instances.push(spawned.instance_id);
            }
            reservation.calls.clear();
        }
        state.pending_calls = 0;
        state.local_connections = 0;
        state.reservations.clear();
        instances
    }

    pub(crate) fn put_reply_chunk(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        call_id: &HarnessMcpCallId,
        offset: u32,
        final_chunk: bool,
        chunk_hex: &HarnessMcpReplyChunkHexV1,
    ) -> Result<(u32, bool), HarnessMcpProxyError> {
        let decoded = decode_hex(chunk_hex.as_str()).ok_or(HarnessMcpProxyError::Conflict)?;
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get_mut(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, activation_digest, unix_time_ms())?;
        validate_activation(reservation, record_id, session)?;
        let call = reservation.calls.get_mut(call_id).ok_or(HarnessMcpProxyError::CallNotFound)?;
        if call.binding.record_id != *record_id || call.binding.session != *session {
            return Err(HarnessMcpProxyError::BindingMismatch);
        }
        if call.terminal.is_none() {
            return Err(HarnessMcpProxyError::CallNotFound);
        }
        if offset != call.next_offset {
            return Err(HarnessMcpProxyError::ChunkOutOfOrder);
        }
        if call.bytes.len().saturating_add(decoded.len()) > MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES {
            return Err(HarnessMcpProxyError::ResponseTooLarge);
        }
        call.bytes.extend_from_slice(&decoded);
        call.next_offset = call.next_offset.saturating_add(decoded.len() as u32);
        let next_offset = call.next_offset;
        if !final_chunk {
            return Ok((next_offset, false));
        }
        let reply: HarnessMcpLocalReplyV1 = serde_json::from_slice(&call.bytes)
            .map_err(|_| HarnessMcpProxyError::Conflict)?;
        reply.validate().map_err(|_| HarnessMcpProxyError::Conflict)?;
        let terminal = call.terminal.take().ok_or(HarnessMcpProxyError::CallNotFound)?;
        reservation.calls.remove(call_id);
        state.pending_calls = state.pending_calls.saturating_sub(1);
        let _ = terminal.send(reply);
        Ok((next_offset, true))
    }

    pub(crate) fn reject_call(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        call_id: &HarnessMcpCallId,
        reason: HarnessMcpRejectReasonV1,
    ) -> Result<(), HarnessMcpProxyError> {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state.reservations.get_mut(reservation_id)
            .ok_or(HarnessMcpProxyError::NotFound)?;
        validate_reservation(reservation, activation_digest, unix_time_ms())?;
        validate_activation(reservation, record_id, session)?;
        let mut call = reservation.calls.remove(call_id).ok_or(HarnessMcpProxyError::CallNotFound)?;
        state.pending_calls = state.pending_calls.saturating_sub(1);
        let terminal = call.terminal.take().ok_or(HarnessMcpProxyError::CallNotFound)?;
        let _ = terminal.send(HarnessMcpLocalReplyV1::Error { error: reject_error(reason) });
        Ok(())
    }

    async fn accept_loop(
        self,
        reservation_id: HarnessMcpReservationId,
        token: HarnessMcpLocalToken,
        runtime: Arc<ReservationRuntime>,
        mut listener: OwnerOnlyLocalListener,
        expires_at_unix_ms: u64,
    ) {
        loop {
            let notified = runtime.changed.notified();
            let (revoked, activated) = {
                let state = runtime.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.revoked, state.activation.is_some())
            };
            if revoked {
                break;
            }
            if !activated && expires_at_unix_ms <= unix_time_ms() {
                revoke_runtime(&runtime);
                break;
            }
            let stream = tokio::select! {
                stream = listener.accept() => match stream { Ok(stream) => stream, Err(_) => break },
                _ = notified => continue,
                _ = tokio::time::sleep(Duration::from_millis(
                    expires_at_unix_ms.saturating_sub(unix_time_ms()).max(1)
                )), if !activated => {
                    revoke_runtime(&runtime);
                    break;
                },
            };
            if !self.acquire_connection_slot(&reservation_id) {
                drop(stream);
                continue;
            }
            let registry = self.clone();
            let reservation_id = reservation_id.clone();
            let token = token.clone();
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move {
                let _ = registry.serve_local_connection(
                    stream,
                    reservation_id.clone(),
                    token,
                    runtime,
                ).await;
                registry.release_connection_slot(&reservation_id);
            });
        }
    }

    async fn serve_local_connection(
        &self,
        mut stream: LocalServerStream,
        reservation_id: HarnessMcpReservationId,
        token: HarnessMcpLocalToken,
        runtime: Arc<ReservationRuntime>,
    ) -> Result<(), ()> {
        let request = timeout(
            LOCAL_READ_TIMEOUT,
            read_json_frame_limited_body_timeout::<_, HarnessMcpLocalRequestV1>(
                &mut stream,
                MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES,
                LOCAL_READ_TIMEOUT,
            ),
        ).await.map_err(|_| ())?.map_err(|_| ())?;
        request.validate().map_err(|_| ())?;
        if !constant_time_equal(request.token.expose().as_bytes(), token.expose().as_bytes()) {
            write_local_reply(&mut stream, HarnessMcpLocalReplyV1::Error {
                error: HarnessReadHostErrorV1::Unauthorized,
            }).await.map_err(|_| ())?;
            return Ok(());
        }
        let expires_at_unix_ms = self.reservation_expiry(&reservation_id).ok_or(())?;
        let activation_wait = async {
            loop {
                let notified = runtime.changed.notified();
                let activation = {
                    let state = runtime.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.revoked { return Err(()); }
                    state.activation.clone()
                };
                if let Some(activation) = activation { return Ok(activation); }
                notified.await;
            }
        };
        let activation = timeout(
            Duration::from_millis(expires_at_unix_ms.saturating_sub(unix_time_ms()).max(1)),
            activation_wait,
        ).await.map_err(|_| ())??;
        if !client_is_provider_descendant(&stream, activation.provider_root_pid) {
            write_local_reply(&mut stream, HarnessMcpLocalReplyV1::Error {
                error: HarnessReadHostErrorV1::Unauthorized,
            }).await.map_err(|_| ())?;
            return Ok(());
        }
        let (call_id, receiver, deadline_unix_ms) = self.begin_call(
            &reservation_id,
            &activation,
            request.request,
        ).map_err(|_| ())?;
        let remaining = Duration::from_millis(deadline_unix_ms.saturating_sub(unix_time_ms()).max(1));
        let reply = match timeout(remaining, receiver).await {
            Ok(Ok(reply)) => reply,
            _ => {
                self.drop_call(&reservation_id, &call_id);
                HarnessMcpLocalReplyV1::Error { error: HarnessReadHostErrorV1::Deadline }
            }
        };
        write_local_reply(&mut stream, reply).await.map_err(|_| ())
    }

    fn begin_call(
        &self,
        reservation_id: &HarnessMcpReservationId,
        activation: &ActivationBinding,
        request: crate::protocol::HarnessReadRequestV1,
    ) -> Result<(HarnessMcpCallId, oneshot::Receiver<HarnessMcpLocalReplyV1>, u64), HarnessMcpProxyError> {
        let sink = self.event_sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
            .ok_or(HarnessMcpProxyError::Unavailable)?;
        let deadline_unix_ms = unix_time_ms().saturating_add(MAX_HARNESS_MCP_CALL_DEADLINE_MS);
        let (sender, receiver) = oneshot::channel();
        let (call_id, event) = {
            let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.pending_calls >= MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE {
                return Err(HarnessMcpProxyError::Unavailable);
            }
            let reservation = state.reservations.get_mut(reservation_id)
                .ok_or(HarnessMcpProxyError::NotFound)?;
            validate_reservation(reservation, &reservation.activation_digest.clone(), unix_time_ms())?;
            if reservation.calls.len() >= MAX_HARNESS_MCP_PENDING_CALLS_PER_SESSION {
                return Err(HarnessMcpProxyError::Unavailable);
            }
            validate_activation(reservation, &activation.record_id, &activation.session)?;
            let call_id = (0..4).find_map(|_| {
                new_call_id().ok().filter(|candidate| !reservation.calls.contains_key(candidate))
            }).ok_or(HarnessMcpProxyError::Unavailable)?;
            let activation_digest = {
                reservation.calls.insert(call_id.clone(), PendingCall {
                    binding: activation.clone(),
                    next_offset: 0,
                    bytes: Vec::new(),
                    terminal: Some(sender),
                });
                reservation.activation_digest.clone()
            };
            state.pending_calls += 1;
            let event = NodeEvent::HarnessMcpReadCall {
                reservation_id: reservation_id.clone(),
                activation_digest,
                record_id: activation.record_id.clone(),
                session: activation.session.clone(),
                call_id: call_id.clone(),
                request,
                deadline_unix_ms,
            };
            (call_id, event)
        };
        sink(event);
        Ok((call_id, receiver, deadline_unix_ms))
    }

    fn reservation_expiry(&self, reservation_id: &HarnessMcpReservationId) -> Option<u64> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .reservations.get(reservation_id).map(|reservation| reservation.expires_at_unix_ms)
    }

    fn acquire_connection_slot(&self, reservation_id: &HarnessMcpReservationId) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.local_connections >= MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE {
            return false;
        }
        let Some(reservation) = state.reservations.get_mut(reservation_id) else {
            return false;
        };
        if reservation.local_connections >= MAX_HARNESS_MCP_PENDING_CALLS_PER_SESSION {
            return false;
        }
        reservation.local_connections += 1;
        state.local_connections += 1;
        true
    }

    fn release_connection_slot(&self, reservation_id: &HarnessMcpReservationId) {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let released = state.reservations.get_mut(reservation_id).is_some_and(|reservation| {
            if reservation.local_connections == 0 { return false; }
            reservation.local_connections -= 1;
            true
        });
        if released {
            state.local_connections = state.local_connections.saturating_sub(1);
        }
    }

    fn drop_call(&self, reservation_id: &HarnessMcpReservationId, call_id: &HarnessMcpCallId) {
        let mut state = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(reservation) = state.reservations.get_mut(reservation_id) {
            if reservation.calls.remove(call_id).is_some() {
                state.pending_calls = state.pending_calls.saturating_sub(1);
            }
        }
    }
}

fn validate_reservation(
    reservation: &Reservation,
    activation_digest: &HarnessMcpActivationDigest,
    now: u64,
) -> Result<(), HarnessMcpProxyError> {
    if reservation.activation_digest != *activation_digest || reservation.aborted {
        return Err(HarnessMcpProxyError::Conflict);
    }
    if reservation.activation.is_none() && reservation.expires_at_unix_ms <= now {
        return Err(HarnessMcpProxyError::Expired);
    }
    Ok(())
}

fn prune_registry(state: &mut RegistryState, now: u64) {
    state.tombstones.retain(|_, tombstone| tombstone.expires_at_unix_ms > now);
    let expired: Vec<_> = state.reservations.iter()
        .filter_map(|(reservation_id, reservation)| {
            (reservation.aborted
                || (reservation.activation.is_none() && reservation.expires_at_unix_ms <= now))
                .then(|| reservation_id.clone())
        })
        .collect();
    for reservation_id in expired {
        if let Some(reservation) = state.reservations.remove(&reservation_id) {
            revoke_runtime(&reservation.runtime);
            state.pending_calls = state.pending_calls.saturating_sub(reservation.calls.len());
            state.local_connections = state.local_connections.saturating_sub(reservation.local_connections);
            state.tombstones.insert(reservation_id, ReservationTombstone {
                activation_digest: reservation.activation_digest,
                expires_at_unix_ms: now.saturating_add(MAX_HARNESS_MCP_RESERVATION_TTL_MS),
            });
        }
    }
}

fn validate_activation(
    reservation: &Reservation,
    record_id: &SessionRecordId,
    session: &SessionAddress,
) -> Result<(), HarnessMcpProxyError> {
    match reservation.activation.as_ref() {
        Some(binding) if binding.record_id == *record_id && binding.session == *session => Ok(()),
        Some(_) => Err(HarnessMcpProxyError::BindingMismatch),
        None => Err(HarnessMcpProxyError::NotActivated),
    }
}

fn revoke_runtime(runtime: &ReservationRuntime) {
    let mut state = runtime.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.revoked = true;
    state.activation = None;
    drop(state);
    runtime.changed.notify_waiters();
}

async fn write_local_reply(
    stream: &mut LocalServerStream,
    reply: HarnessMcpLocalReplyV1,
) -> io::Result<()> {
    reply.validate().map_err(|_| io::Error::other("invalid local reply"))?;
    write_json_frame_limited(stream, &reply, MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES)
        .await
        .map_err(io::Error::other)
}

fn reject_error(reason: HarnessMcpRejectReasonV1) -> HarnessReadHostErrorV1 {
    match reason {
        HarnessMcpRejectReasonV1::Unauthorized => HarnessReadHostErrorV1::Unauthorized,
        HarnessMcpRejectReasonV1::Unavailable | HarnessMcpRejectReasonV1::Internal => {
            HarnessReadHostErrorV1::Internal
        }
        HarnessMcpRejectReasonV1::InvalidRequest => HarnessReadHostErrorV1::InvalidRequest,
        HarnessMcpRejectReasonV1::NotFoundOrDenied => HarnessReadHostErrorV1::NotFoundOrDenied,
        HarnessMcpRejectReasonV1::ResponseTooLarge => HarnessReadHostErrorV1::TooLarge,
        HarnessMcpRejectReasonV1::Deadline => HarnessReadHostErrorV1::Deadline,
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    value.as_bytes().chunks_exact(2).map(|pair| {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        Some((high << 4) | low)
    }).collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn new_token() -> Result<HarnessMcpLocalToken, HarnessMcpProxyError> {
    let bytes = random_nonce().map_err(|_| HarnessMcpProxyError::Unavailable)?;
    HarnessMcpLocalToken::new(format!("g4ah3_{}", lower_hex(&bytes)))
        .map_err(|_| HarnessMcpProxyError::Unavailable)
}

fn new_call_id() -> Result<HarnessMcpCallId, HarnessMcpProxyError> {
    let bytes = random_nonce().map_err(|_| HarnessMcpProxyError::Unavailable)?;
    HarnessMcpCallId::new(format!("hmcpcall_{}", lower_hex(&bytes[..12])))
        .map_err(|_| HarnessMcpProxyError::Unavailable)
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes { let _ = write!(&mut value, "{byte:02x}"); }
    value
}

async fn prebind_listener() -> Result<(PathBuf, OwnerOnlyLocalListener), HarnessMcpProxyError> {
    for _ in 0..8 {
        let bytes = random_nonce().map_err(|_| HarnessMcpProxyError::Unavailable)?;
        let endpoint = local_endpoint(&lower_hex(&bytes[..12]))?;
        match OwnerOnlyLocalListener::bind(&endpoint).await {
            Ok(listener) => return Ok((endpoint, listener)),
            Err(error) if matches!(error.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied) => continue,
            Err(_) => return Err(HarnessMcpProxyError::Unavailable),
        }
    }
    Err(HarnessMcpProxyError::Unavailable)
}

#[cfg(windows)]
fn local_endpoint(nonce: &str) -> Result<PathBuf, HarnessMcpProxyError> {
    Ok(PathBuf::from(format!(r"\\.\pipe\gate4agent-h3b-{}-{nonce}", std::process::id())))
}

#[cfg(unix)]
fn local_endpoint(nonce: &str) -> Result<PathBuf, HarnessMcpProxyError> {
    use std::os::unix::fs::PermissionsExt;
    let parent = std::env::temp_dir().join(format!("gate4agent-h3b-{}", std::process::id()));
    std::fs::create_dir_all(&parent).map_err(|_| HarnessMcpProxyError::Unavailable)?;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| HarnessMcpProxyError::Unavailable)?;
    Ok(parent.join(format!("{nonce}.sock")))
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
        .as_millis().min(u64::MAX as u128) as u64
}

fn hash_file(path: &Path) -> Result<[u8; 32], HarnessMcpProxyError> {
    let mut file = std::fs::File::open(path).map_err(|_| HarnessMcpProxyError::Unavailable)?;
    let mut context = DigestContext::new(&SHA256);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| HarnessMcpProxyError::Unavailable)?;
        if read == 0 { break; }
        context.update(&buffer[..read]);
    }
    let digest = context.finish();
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(digest.as_ref());
    Ok(bytes)
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn metadata_is_reparse(_metadata: &std::fs::Metadata) -> bool { false }

#[cfg(windows)]
fn helper_file_identity(metadata: &std::fs::Metadata) -> HelperFileIdentity {
    use std::os::windows::fs::MetadataExt;
    HelperFileIdentity {
        first: metadata.creation_time(),
        second: metadata.last_write_time(),
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() { return false; }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(unix)]
fn helper_file_identity(metadata: &std::fs::Metadata) -> HelperFileIdentity {
    use std::os::unix::fs::MetadataExt;
    HelperFileIdentity { first: metadata.dev(), second: metadata.ino() }
}

#[cfg(windows)]
fn client_is_provider_descendant(stream: &LocalServerStream, provider_root_pid: u32) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let mut client_pid = 0_u32;
    if unsafe { GetNamedPipeClientProcessId(stream.as_raw_handle().cast(), &mut client_pid) } == 0
        || client_pid == 0
    {
        return false;
    }
    if client_pid == provider_root_pid { return true; }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE { return false; }
    let mut parents = BTreeMap::new();
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    let mut count = 0usize;
    while ok && count < 65_536 {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        count += 1;
    }
    unsafe { CloseHandle(snapshot); }
    let mut current = client_pid;
    for _ in 0..64 {
        let Some(parent) = parents.get(&current).copied() else { return false };
        if parent == provider_root_pid { return true; }
        if parent == 0 || parent == current { return false; }
        current = parent;
    }
    false
}

#[cfg(unix)]
fn client_is_provider_descendant(stream: &LocalServerStream, provider_root_pid: u32) -> bool {
    stream.peer_cred().ok().and_then(|credential| credential.pid())
        .is_some_and(|pid| pid as u32 == provider_root_pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_wire::connect_local_stream;
    use tokio::io::AsyncWriteExt as _;

    #[test]
    fn reply_chunks_enforce_offset_overflow_and_single_terminal() {
        assert_eq!(decode_hex("0001ff").unwrap(), vec![0, 1, 255]);
        assert!(decode_hex("0g").is_none());
        assert_eq!(MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES, 1024 * 1024);
    }

    #[test]
    fn runtime_revocation_is_idempotent() {
        let runtime = ReservationRuntime {
            state: Mutex::new(RuntimeState { activation: None, revoked: false }),
            changed: Notify::new(),
        };
        revoke_runtime(&runtime);
        revoke_runtime(&runtime);
        assert!(runtime.state.lock().unwrap().revoked);
    }

    #[tokio::test]
    async fn local_capability_from_one_reservation_is_rejected_by_another_endpoint() {
        let helper_program = ReviewedHarnessMcpProgram::review(std::env::current_exe().unwrap())
            .expect("review current test executable as fixture helper");
        let registry = HarnessMcpProxyRegistry::new(helper_program);
        let spec = SpawnSpec {
            target: crate::protocol::SpawnTarget {
                node_id: crate::protocol::NodeId::new("h3b-node-test").unwrap(),
                workspace_id: crate::protocol::WorkspaceId::new("primary").unwrap(),
                worktree_id: None,
            },
            profile_id: crate::protocol::SpawnProfileId::new("codex").unwrap(),
            expected_profile_revision: crate::protocol::SpawnProfileRevision::new("r1").unwrap(),
            overrides: crate::protocol::SpawnOverrides::default(),
            deadline_ms: crate::protocol::SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: crate::protocol::SpawnIdempotencyKey::new("h3b-cross-reservation").unwrap(),
            required_capabilities: crate::protocol::SpawnRequiredCapabilities::default(),
        };
        let reservation_a = HarnessMcpReservationId::new(format!("hmcpres_{}", "a".repeat(24))).unwrap();
        let reservation_b = HarnessMcpReservationId::new(format!("hmcpres_{}", "b".repeat(24))).unwrap();
        let digest_a = HarnessMcpActivationDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap();
        let digest_b = HarnessMcpActivationDigest::new(format!("sha256:{}", "b".repeat(64))).unwrap();
        let expires_at_unix_ms = unix_time_ms().saturating_add(30_000);
        registry.arm(reservation_a.clone(), digest_a.clone(), spec.clone(), expires_at_unix_ms)
            .await.unwrap();
        registry.arm(reservation_b.clone(), digest_b.clone(), spec, expires_at_unix_ms)
            .await.unwrap();

        let (endpoint_b, token_a) = {
            let state = registry.inner.lock().unwrap();
            (
                state.reservations.get(&reservation_b).unwrap().endpoint.clone(),
                state.reservations.get(&reservation_a).unwrap().token.clone(),
            )
        };
        let request = HarnessMcpLocalRequestV1 {
            version: 1,
            token: token_a,
            request: crate::protocol::HarnessReadRequestV1::ContextGet,
        };
        let mut stream = connect_local_stream(&endpoint_b).await.unwrap();
        write_json_frame_limited(&mut stream, &request, MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES)
            .await.unwrap();
        stream.shutdown().await.unwrap();
        let reply: HarnessMcpLocalReplyV1 = read_json_frame_limited_body_timeout(
            &mut stream,
            MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES,
            LOCAL_READ_TIMEOUT,
        ).await.unwrap();
        assert_eq!(reply, HarnessMcpLocalReplyV1::Error {
            error: HarnessReadHostErrorV1::Unauthorized,
        });

        let state = registry.inner.lock().unwrap();
        let reservation = state.reservations.get(&reservation_b).unwrap();
        assert_eq!(state.pending_calls, 0);
        assert!(reservation.calls.is_empty());
        assert!(reservation.activation.is_none());
        drop(state);
        registry.abort(&reservation_a, &digest_a).unwrap();
        registry.abort(&reservation_b, &digest_b).unwrap();
    }
}
