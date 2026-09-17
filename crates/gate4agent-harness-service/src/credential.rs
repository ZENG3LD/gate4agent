use gate4agent_harness_engine::HarnessEngine;
use gate4agent_harness_api::{
    HarnessReadApiError, HarnessReadCredential, HARNESS_MCP_AUDIENCE,
};
use gate4agent_harness_protocol::{
    HarnessRevision, HarnessRunId, HarnessRunLifecycleV1, HarnessSelectorV1,
    HarnessSessionIdentityV1, SessionGrantId, SessionGrantStateV1,
};
use gate4agent_node_wire::{local_hmac_sha256, proofs_match, random_nonce};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const HARNESS_CREDENTIAL_TTL_MAX_MS: u64 = 300_000;
const TOKEN_PREFIX: &str = "g4ah2_";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialBindingV1 {
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub actor_run_id: HarnessRunId,
    pub node_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub node_incarnation: HarnessSelectorV1,
    pub record_id: HarnessSelectorV1,
    pub instance_id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifiedCredentialV1 {
    pub(crate) binding: CredentialBindingV1,
    audience: String,
    issued_at_unix_ms: u64,
    expires_at_unix_ms: u64,
    nonce: String,
}

pub struct CredentialAuthority {
    secret: [u8; 32],
}

impl CredentialAuthority {
    pub fn new() -> Result<Self, CredentialError> {
        Ok(Self { secret: random_nonce().map_err(CredentialError::Crypto)? })
    }

    pub fn mint(
        &self,
        engine: &HarnessEngine,
        binding: CredentialBindingV1,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<HarnessReadCredential, CredentialError> {
        validate_times(issued_at_unix_ms, expires_at_unix_ms, issued_at_unix_ms)?;
        validate_binding(engine, &binding)?;
        let nonce = encode_hex(&random_nonce().map_err(CredentialError::Crypto)?);
        let claims = VerifiedCredentialV1 {
            binding,
            audience: HARNESS_MCP_AUDIENCE.to_owned(),
            issued_at_unix_ms,
            expires_at_unix_ms,
            nonce,
        };
        let payload = serde_json::to_vec(&claims).map_err(CredentialError::Json)?;
        let proof = local_hmac_sha256(&self.secret, &payload).map_err(CredentialError::Crypto)?;
        let token = format!("{TOKEN_PREFIX}{}.{}", encode_hex(&payload), encode_hex(&proof));
        Ok(HarnessReadCredential::parse(token)?)
    }

    pub(crate) fn verify(
        &self,
        engine: &HarnessEngine,
        credential: &HarnessReadCredential,
        now_unix_ms: u64,
    ) -> Result<VerifiedCredentialV1, CredentialError> {
        let encoded = credential.expose().strip_prefix(TOKEN_PREFIX)
            .ok_or(CredentialError::Malformed)?;
        let (payload_hex, proof_hex) = encoded.split_once('.').ok_or(CredentialError::Malformed)?;
        let payload = decode_hex(payload_hex)?;
        let supplied = decode_hex(proof_hex)?;
        let supplied: [u8; 32] = supplied.try_into().map_err(|_| CredentialError::Malformed)?;
        let expected = local_hmac_sha256(&self.secret, &payload).map_err(CredentialError::Crypto)?;
        if !proofs_match(&supplied, &expected) {
            return Err(CredentialError::Rejected);
        }
        let claims: VerifiedCredentialV1 =
            serde_json::from_slice(&payload).map_err(CredentialError::Json)?;
        if claims.audience != HARNESS_MCP_AUDIENCE {
            return Err(CredentialError::Rejected);
        }
        validate_times(claims.issued_at_unix_ms, claims.expires_at_unix_ms, now_unix_ms)?;
        validate_binding(engine, &claims.binding)?;
        Ok(claims)
    }
}

fn validate_times(issued: u64, expires: u64, now: u64) -> Result<(), CredentialError> {
    if issued == 0
        || expires <= issued
        || expires.saturating_sub(issued) > HARNESS_CREDENTIAL_TTL_MAX_MS
        || now < issued
        || now >= expires
    {
        return Err(CredentialError::Expired);
    }
    Ok(())
}

fn validate_binding(
    engine: &HarnessEngine,
    binding: &CredentialBindingV1,
) -> Result<(), CredentialError> {
    let grant = engine.grant(&binding.grant_id).ok_or(CredentialError::Rejected)?;
    if grant.state != SessionGrantStateV1::Active
        || grant.revision != binding.grant_revision
        || grant.actor_run_id != binding.actor_run_id
    {
        return Err(CredentialError::Rejected);
    }
    let run = engine.run(&binding.actor_run_id).ok_or(CredentialError::Rejected)?;
    if !matches!(run.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting)
        || run.intent.node_id != binding.node_id
        || run.intent.workspace_id != binding.workspace_id
    {
        return Err(CredentialError::Rejected);
    }
    let session_matches = matches!(
        run.binding.as_ref(),
        Some(session) if session.node_id == binding.node_id
            && session.workspace_id == binding.workspace_id
            && session.node_incarnation == binding.node_incarnation
            && matches!(
                &session.session,
                HarnessSessionIdentityV1::Managed {
                    record_id,
                    active_session: Some(active),
                } if record_id == &binding.record_id
                    && active.instance_id == binding.instance_id
                    && active.generation == binding.generation
            )
    );
    if !session_matches
        || !grant.allows_target(
            &run.intent.node_id,
            &run.intent.workspace_id,
            &run.intent.provider_profile,
            run.intent.mode,
        )
    {
        return Err(CredentialError::Rejected);
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, CredentialError> {
    if encoded.len() % 2 != 0
        || !encoded.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(CredentialError::Malformed);
    }
    encoded.as_bytes().chunks_exact(2).map(|pair| {
        let text = std::str::from_utf8(pair).map_err(|_| CredentialError::Malformed)?;
        u8::from_str_radix(text, 16).map_err(|_| CredentialError::Malformed)
    }).collect()
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential is malformed")]
    Malformed,
    #[error("credential is expired or not yet valid")]
    Expired,
    #[error("credential authority rejected the binding")]
    Rejected,
    #[error("credential cryptography failed: {0}")]
    Crypto(String),
    #[error("credential encoding failed: {0}")]
    Json(serde_json::Error),
    #[error(transparent)]
    Api(#[from] HarnessReadApiError),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gate4agent_harness_engine::{
        HarnessEngineCheckpointV1, HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
    };
    use gate4agent_harness_protocol::*;

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn task_id() -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap()
    }

    fn run_id() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap()
    }

    fn grant_id() -> SessionGrantId {
        SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap()
    }

    fn operation_id() -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", "a".repeat(24))).unwrap()
    }

    fn incarnation_selector() -> HarnessSelectorV1 {
        selector(&gate4agent_node_protocol::NodeIncarnationId::from_bytes([4; 16]).to_string())
    }

    pub(crate) fn engine(
        grant_revision: u64,
        grant_state: SessionGrantStateV1,
        generation: u64,
        lifecycle: HarnessRunLifecycleV1,
    ) -> HarnessEngine {
        let task = HarnessTaskV1 {
            task_id: task_id(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "credential fixture".to_owned(),
            body: String::new(),
            creator: HarnessActorV1::User { actor_id: selector("operator") },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![run_id()],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        };
        let result_disposition = (lifecycle == HarnessRunLifecycleV1::Completed)
            .then_some(HarnessResultDispositionV1::Succeeded);
        let run = HarnessRunV1 {
            run_id: run_id(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id(),
            operation_id: operation_id(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude-default"),
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
                node_incarnation: incarnation_selector(),
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-a"),
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id: 7,
                        generation,
                    }),
                },
            }),
            lifecycle,
            result_disposition,
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        };
        let grant = SessionGrantV1 {
            grant_id: grant_id(),
            revision: HarnessRevision::new(grant_revision).unwrap(),
            actor_run_id: run_id(),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                provider_profile: selector("claude-default"),
                mode: HarnessExecutionModeV1::Pty,
            }],
            allowed_delivery_bundles: Vec::new(),
            maximum_child_count: 0,
            maximum_child_depth: 0,
            operation_timeouts: HarnessOperationTimeoutsV1 {
                dispatch_ms: 1_000,
                wait_ms: 1_000,
                reconciliation_ms: 1_000,
            },
            task_permissions: HarnessTaskPermissionsV1 {
                read: true,
                create: false,
                mutate: false,
                request_run: false,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::Summary,
            context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
            state: grant_state,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10 + grant_revision,
        };
        let operation = HarnessOperationV1 {
            operation_id: operation_id(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User { actor_id: selector("operator") },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id()),
            run_id: Some(run_id()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}", "a".repeat(24)
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
            dispatched_at_unix_ms: Some(10),
            finished_at_unix_ms: Some(10),
        };
        HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![task],
            runs: vec![run],
            grants: vec![grant],
            operations: vec![operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap()
    }

    pub(crate) fn binding(revision: u64, generation: u64) -> CredentialBindingV1 {
        CredentialBindingV1 {
            grant_id: grant_id(),
            grant_revision: HarnessRevision::new(revision).unwrap(),
            actor_run_id: run_id(),
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation: incarnation_selector(),
            record_id: selector("record-a"),
            instance_id: 7,
            generation,
        }
    }

    #[test]
    fn credential_is_invalid_after_grant_session_or_authority_change() {
        let active = engine(1, SessionGrantStateV1::Active, 1, HarnessRunLifecycleV1::Running);
        let authority = CredentialAuthority::new().unwrap();
        let credential = authority.mint(&active, binding(1, 1), 100, 200).unwrap();
        assert!(authority.verify(&active, &credential, 150).is_ok());
        assert!(authority.verify(
            &engine(2, SessionGrantStateV1::Active, 1, HarnessRunLifecycleV1::Running),
            &credential,
            150,
        ).is_err());
        assert!(authority.verify(
            &engine(1, SessionGrantStateV1::Revoked, 1, HarnessRunLifecycleV1::Running),
            &credential,
            150,
        ).is_err());
        assert!(authority.verify(
            &engine(1, SessionGrantStateV1::Active, 2, HarnessRunLifecycleV1::Running),
            &credential,
            150,
        ).is_err());
        assert!(CredentialAuthority::new().unwrap().verify(&active, &credential, 150).is_err());
        assert!(authority.verify(&active, &credential, 200).is_err());
    }

    #[test]
    fn credential_rejects_terminal_run_and_noncanonical_encoding() {
        let completed = engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Completed,
        );
        assert!(CredentialAuthority::new()
            .unwrap()
            .mint(&completed, binding(1, 1), 100, 200)
            .is_err());

        let active = engine(1, SessionGrantStateV1::Active, 1, HarnessRunLifecycleV1::Running);
        let authority = CredentialAuthority::new().unwrap();
        let credential = authority.mint(&active, binding(1, 1), 100, 200).unwrap();
        let mut bytes = credential.expose().as_bytes().to_vec();
        let index = bytes.iter().position(|byte| matches!(byte, b'a'..=b'f')).unwrap();
        bytes[index] = bytes[index].to_ascii_uppercase();
        assert!(matches!(
            HarnessReadCredential::parse(String::from_utf8(bytes).unwrap()),
            Err(HarnessReadApiError::MalformedCredential)
        ));
    }
}
