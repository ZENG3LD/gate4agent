//! Bounded wire contract for the local Gate4Agent node.

pub use gate4agent_types::{
    AdapterFamily, AdapterId, AgentId, HistoryCandidateSummary, NativeSessionCatalogScope,
    NativeSessionCatalogSummary, NativeSessionCatalogWindow, NativeSessionExternalGroup,
    NativeSessionExternalGroupKind, SessionRecordPreview, HISTORY_DISCOVERY_LIMIT_MAX,
    NATIVE_SESSION_CATALOG_LIMIT_MAX,
    NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX,
};
pub use gate4agent_observation_protocol::{
    ObservationCapabilitiesV1, ObservationEvidenceV1, ObservationInteractionOutcomeV1,
    ObservationKindV1, ObservationSourceFamilyV1, ObservationTodoItemV1,
    ObservationTodoStateV1, ObservationV1,
};
pub use gate4agent_harness_api::{
    HarnessReadHostErrorV1, HarnessReadRequestV1, HarnessReadResponseV1,
};
use gate4agent_types::{
    AgentInstanceId, ControlEvent, ProviderActivity, ProviderSessionIdentity, SessionGeneration,
    SessionSnapshot, TerminalControl, TerminalFrame, TerminalSize,
};
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io;
use std::str::FromStr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Duration};

pub const NODE_PROTOCOL_VERSION: u16 = 11;
pub const NODE_STATE_SCHEMA_V1: u16 = 1;
pub const NODE_STATE_SCHEMA_V2: u16 = 2;
pub const NODE_STATE_SCHEMA_V3: u16 = 3;
pub const NODE_STATE_SCHEMA_V4: u16 = 4;
pub const NODE_STATE_SCHEMA_V5: u16 = 5;
pub const NODE_STATE_SCHEMA_V6: u16 = 6;
pub const NODE_STATE_SCHEMA_V7: u16 = 7;
pub const NODE_STATE_SCHEMA_V8: u16 = 8;
pub const NODE_STATE_SCHEMA_V9: u16 = 9;
pub const NODE_STATE_SCHEMA_V10: u16 = 10;
pub const NODE_COMPATIBILITY_METADATA_CAPABILITY: &str = "compatibility.metadata";
pub const NODE_OPAQUE_UNIX_PATH_CAPABILITY: &str = "path.opaque-unix-bytes-v1";
pub const NODE_REPOSITORY_PATH_CAPABILITY: &str = "repository-path-v1";
pub const NODE_WORKSPACE_FILE_READ_CAPABILITY: &str = "workspace-file-read-v1";
pub const NODE_WORKSPACE_FILE_WRITE_CAPABILITY: &str = "workspace-file-write-v1";
pub const NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY: &str = "workspace-entry-create-v1";
pub const NODE_GIT_READ_CAPABILITY: &str = "git-read-v1";
pub const NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY: &str = "provider-contract-manifest-v1";
pub const NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY: &str = "provider-runtime-status-v1";
pub const NODE_PROVIDER_ID_OPEN_CAPABILITY: &str = "provider-id.open-v1";
pub const NODE_TERMINAL_FRAME_EVENTS_CAPABILITY: &str = "terminal-frame-events-v1";
pub const NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY: &str =
    "spawn-spec.defaults-overrides-v1";
pub const NODE_SPAWN_PROFILE_REVISION_CAPABILITY: &str =
    "spawn-spec.profile-revision-v1";
pub const NODE_WORKTREE_SELECTION_CAPABILITY: &str = "worktree-selection-v1";
pub const NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY: &str =
    "managed-worktree-lifecycle-v1";
pub const NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY: &str =
    "managed-worktree-spawn-v2";
pub const NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY: &str =
    "child-environment-profile-v1";
pub const NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY: &str =
    "session-bundle-materialization-v1";
pub const NODE_HISTORY_CONTEXT_PACK_CAPABILITY: &str = "history-context-pack-v1";
pub const NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY: &str =
    "standalone-workspace-lifecycle-v1";
pub const NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY: &str =
    "provider-session-reference-index-v1";
pub const NODE_NATIVE_SESSION_CATALOG_CAPABILITY: &str = "native-session-catalog-v2";
pub const NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY: &str =
    "native-session-catalog-paging-v2";
pub const NODE_NATIVE_SESSION_PREVIEW_CAPABILITY: &str = "native-session-preview-v2";
pub const NODE_NATIVE_SESSION_INDEX_CAPABILITY: &str = "native-session-index-v2";
pub const NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY: &str = "agent-progress-snapshot-v1";
pub const NODE_SESSION_TASK_CORRELATION_CAPABILITY: &str = "session-task-correlation-v1";
pub const NODE_OBSERVATION_EVENTS_CAPABILITY: &str = "observation-events-v1";
pub const NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY: &str =
    "observation-managed-target-v1";
pub const NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY: &str =
    "observation-workflow-detail-v1";
pub const NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY: &str =
    "delivery-bundle-v2-stage-commit";
pub const NODE_HARNESS_MCP_READ_PROXY_CAPABILITY: &str = "harness-mcp-read-proxy-v1";
/// Read-only, paged directory browsing using UTF-8 absolute host paths only.
/// `OpaqueHostPath::UnixBytes` is outside this capability revision.
pub const CAPABILITY_HOST_DIRECTORY_BROWSE_V1: &str = "host-directory-browse-v1";
pub const SPAWN_RUNTIME_RAW_PTY_LIFECYCLE: &str = "raw-pty-lifecycle";
pub const SPAWN_RUNTIME_SEMANTIC_READINESS: &str = "semantic-readiness";
pub const SPAWN_RUNTIME_STRUCTURED_PROMPT: &str = "structured-prompt";
pub const SPAWN_RUNTIME_PROVIDER_SESSION_IDENTITY: &str = "provider-session-identity";
pub const SPAWN_RUNTIME_SEMANTIC_RESUME: &str = "semantic-resume";
pub const NODE_LEGACY_PROVIDER_IDS: [&str; 3] = ["claude", "codex", "kimi"];
pub const MAX_NODE_IDENTIFIER_BYTES: usize = 64;
pub const MAX_COMPATIBILITY_IDENTIFIER_BYTES: usize = 64;
pub const MAX_PROVIDER_RUNTIME_VERSION_BYTES: usize = MAX_COMPATIBILITY_IDENTIFIER_BYTES;
pub const MAX_PROVIDER_RUNTIME_CONTRACT_ID_BYTES: usize = MAX_COMPATIBILITY_IDENTIFIER_BYTES;
pub const MAX_PROVIDER_CONTRACT_REVISION_BYTES: usize = 128;
pub const MAX_ADAPTER_CONTRACT_REVISION_BYTES: usize = 128;
pub const MAX_PROVIDER_IDENTITIES: usize = 16;
pub const MAX_PROVIDER_CONTRACTS: usize = MAX_PROVIDER_IDENTITIES;
pub const MAX_PROVIDER_RUNTIME_STATUSES: usize = MAX_PROVIDER_IDENTITIES;
pub const MAX_PROVIDER_ADAPTER_CONTRACTS: usize = 32;
pub const MAX_AGENT_PROGRESS_ENTRIES: usize = 128;
pub const MAX_AGENT_PROGRESS_ENTRY_BYTES: usize = 4_096;
pub const MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS: usize = 8;
pub const MAX_AGENT_PROGRESS_TOOL_LABEL_BYTES: usize = 64;
pub const NODE_INCARNATION_ID_BYTES: usize = 16;
pub const TASK_ID_NONCE_BYTES: usize = 12;
pub const MAX_NODE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODE_CLIENT_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_NODE_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_NODE_TERMINAL_BYTES: usize = 64;
pub const MAX_SESSION_DISPLAY_NAME_BYTES: usize = 256;
pub const MAX_SPAWN_PROFILE_ID_BYTES: usize = 64;
pub const MAX_SPAWN_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_PROFILES: usize = 64;
pub const MAX_SPAWN_ENVIRONMENT_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_BUNDLE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_RESOURCE_ID_BYTES: usize = 128;
pub const MAX_SPAWN_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const MAX_SPAWN_REQUIRED_CAPABILITIES: usize = 16;
pub const MAX_CONTEXT_PACK_BYTES: u32 = 256 * 1024;
pub const MAX_CONTEXT_PACK_RETAINED_MESSAGES: u64 =
    gate4agent_types::HISTORY_MESSAGES_MAX as u64;
pub const MAX_WORKTREE_PROFILE_ID_BYTES: usize = 64;
pub const MAX_WORKTREE_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_MANAGED_WORKTREE_LEASE_ID_BYTES: usize = 128;
pub const MAX_MANAGED_WORKTREE_LEASES: usize = 128;
pub const MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE: usize = 64;
pub const MAX_LAUNCH_BUNDLES: usize = 128;
pub const MAX_SPAWN_DEADLINE_MS: u64 = 120_000;
pub const MAX_WORKSPACE_ROOT_BYTES: usize = gate4agent_types::WORKING_DIRECTORY_MAX_BYTES;
pub const MAX_REPOSITORY_PATH_BYTES: usize = 1_024;
pub const MAX_WORKSPACE_FILE_BYTES: usize = 256 * 1024;
pub const MAX_GIT_HISTORY_COMMITS: u16 = 50;
pub const MAX_GIT_DIFF_BYTES: usize = 512 * 1024;
pub const MAX_HOST_DIRECTORY_ENTRIES: usize = 256;
pub const MAX_HOST_DIRECTORY_DISPLAY_NAME_BYTES: usize = 1_024;
pub const MAX_DELIVERY_FILES: usize = 128;
pub const MAX_DELIVERY_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_DELIVERY_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_DELIVERY_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_DELIVERY_CHUNK_RAW_BYTES: usize = 48 * 1024;
pub const MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES: usize = MAX_DELIVERY_CHUNK_RAW_BYTES;
pub const MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES: usize =
    gate4agent_harness_api::HARNESS_READ_REQUEST_MAX_BYTES;
pub const MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES: usize =
    gate4agent_harness_api::HARNESS_READ_RESPONSE_MAX_BYTES;
pub const MAX_HARNESS_MCP_PENDING_CALLS_PER_SESSION: usize = 32;
pub const MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE: usize = 128;
pub const MAX_HARNESS_MCP_RESERVATION_TTL_MS: u64 = 120_000;
pub const MAX_HARNESS_MCP_CALL_DEADLINE_MS: u64 = 3_000;
pub const MAX_HARNESS_MCP_SPAWN_RELAY_DEADLINE_MS: u64 = 125_000;
pub const DELIVERY_STAGE_NONCE_BYTES: usize = 16;
pub const MAX_NODE_HELLO_FRAME_BYTES: usize = 8 * 1024;
pub const NODE_AUTH_NONCE_BYTES: usize = 32;
pub const NODE_AUTH_PROOF_BYTES: usize = 32;
pub const MAX_CONTROLLER_LEASE_MS: u64 = 60_000;
pub const MIN_CONTROLLER_LEASE_MS: u64 = 1_000;
pub const DEFAULT_CONTROLLER_LEASE_MS: u64 = 15_000;

pub fn provider_id_is_legacy(provider: &AgentId) -> bool {
    NODE_LEGACY_PROVIDER_IDS.contains(&provider.as_str())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientRole {
    Operator,
    Observer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRuntimeMode {
    Unavailable,
    RawPassthrough,
    VerifiedSemantic,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderRuntimeVersion(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderRuntimeContractId(String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderRuntimeStatus {
    provider: AgentId,
    mode: ProviderRuntimeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<ProviderRuntimeVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contract_id: Option<ProviderRuntimeContractId>,
}

impl ProviderRuntimeStatus {
    pub fn unavailable(provider: AgentId) -> Self {
        Self {
            provider,
            mode: ProviderRuntimeMode::Unavailable,
            version: None,
            contract_id: None,
        }
    }

    pub fn raw_passthrough(
        provider: AgentId,
        version: Option<ProviderRuntimeVersion>,
    ) -> Self {
        Self {
            provider,
            mode: ProviderRuntimeMode::RawPassthrough,
            version,
            contract_id: None,
        }
    }

    pub fn verified_semantic(
        provider: AgentId,
        version: ProviderRuntimeVersion,
        contract_id: ProviderRuntimeContractId,
    ) -> Self {
        Self {
            provider,
            mode: ProviderRuntimeMode::VerifiedSemantic,
            version: Some(version),
            contract_id: Some(contract_id),
        }
    }

    pub fn provider(&self) -> &AgentId {
        &self.provider
    }

    pub const fn mode(&self) -> ProviderRuntimeMode {
        self.mode
    }

    pub fn version(&self) -> Option<&ProviderRuntimeVersion> {
        self.version.as_ref()
    }

    pub fn contract_id(&self) -> Option<&ProviderRuntimeContractId> {
        self.contract_id.as_ref()
    }

    fn from_wire(
        provider: AgentId,
        mode: ProviderRuntimeMode,
        version: Option<ProviderRuntimeVersion>,
        contract_id: Option<ProviderRuntimeContractId>,
    ) -> Result<Self, ProviderRuntimeStatusError> {
        match (mode, version, contract_id) {
            (ProviderRuntimeMode::Unavailable, None, None) => Ok(Self::unavailable(provider)),
            (ProviderRuntimeMode::RawPassthrough, version, None) => {
                Ok(Self::raw_passthrough(provider, version))
            }
            (ProviderRuntimeMode::VerifiedSemantic, Some(version), Some(contract_id)) => {
                Ok(Self::verified_semantic(provider, version, contract_id))
            }
            (ProviderRuntimeMode::Unavailable, _, _) => {
                Err(ProviderRuntimeStatusError::UnavailableHasMetadata { provider })
            }
            (ProviderRuntimeMode::RawPassthrough, _, Some(_)) => {
                Err(ProviderRuntimeStatusError::RawPassthroughHasContract { provider })
            }
            (ProviderRuntimeMode::VerifiedSemantic, _, _) => {
                Err(ProviderRuntimeStatusError::VerifiedSemanticMissingMetadata { provider })
            }
        }
    }
}

impl<'de> Deserialize<'de> for ProviderRuntimeStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireStatus {
            provider: AgentId,
            mode: ProviderRuntimeMode,
            #[serde(default)]
            version: Option<ProviderRuntimeVersion>,
            #[serde(default)]
            contract_id: Option<ProviderRuntimeContractId>,
        }

        let wire = WireStatus::deserialize(deserializer)?;
        Self::from_wire(wire.provider, wire.mode, wire.version, wire.contract_id)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderRuntimeStatuses(Vec<ProviderRuntimeStatus>);

impl ProviderRuntimeStatuses {
    pub fn new(
        statuses: impl IntoIterator<Item = ProviderRuntimeStatus>,
    ) -> Result<Self, ProviderRuntimeStatusError> {
        let mut bounded = Vec::with_capacity(MAX_PROVIDER_RUNTIME_STATUSES);
        for status in statuses {
            if bounded.len() == MAX_PROVIDER_RUNTIME_STATUSES {
                return Err(ProviderRuntimeStatusError::TooMany {
                    max: MAX_PROVIDER_RUNTIME_STATUSES,
                });
            }
            if bounded
                .iter()
                .any(|existing: &ProviderRuntimeStatus| existing.provider == status.provider)
            {
                return Err(ProviderRuntimeStatusError::DuplicateProvider {
                    provider: status.provider.clone(),
                });
            }
            bounded.push(status);
        }
        bounded.sort_by(|left, right| left.provider.cmp(&right.provider));
        Ok(Self(bounded))
    }

    pub fn as_slice(&self) -> &[ProviderRuntimeStatus] {
        &self.0
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ProviderRuntimeStatus> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn retain(
        &mut self,
        mut predicate: impl FnMut(&ProviderRuntimeStatus) -> bool,
    ) {
        self.0.retain(|status| predicate(status));
    }
}

impl<'de> Deserialize<'de> for ProviderRuntimeStatuses {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StatusesVisitor;

        impl<'de> Visitor<'de> for StatusesVisitor {
            type Value = ProviderRuntimeStatuses;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {MAX_PROVIDER_RUNTIME_STATUSES} unique provider runtime statuses",
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut statuses = Vec::with_capacity(MAX_PROVIDER_RUNTIME_STATUSES);
                while let Some(status) = sequence.next_element::<ProviderRuntimeStatus>()? {
                    if statuses.len() == MAX_PROVIDER_RUNTIME_STATUSES {
                        return Err(serde::de::Error::custom(
                            ProviderRuntimeStatusError::TooMany {
                                max: MAX_PROVIDER_RUNTIME_STATUSES,
                            },
                        ));
                    }
                    if statuses.iter().any(|existing: &ProviderRuntimeStatus| {
                        existing.provider == status.provider
                    }) {
                        return Err(serde::de::Error::custom(
                            ProviderRuntimeStatusError::DuplicateProvider {
                                provider: status.provider.clone(),
                            },
                        ));
                    }
                    statuses.push(status);
                }
                statuses.sort_by(|left, right| left.provider.cmp(&right.provider));
                Ok(ProviderRuntimeStatuses(statuses))
            }
        }

        deserializer.deserialize_seq(StatusesVisitor)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderRuntimeStatusError {
    #[error("provider runtime status exceeds the {max}-provider limit")]
    TooMany { max: usize },
    #[error("provider runtime status contains duplicate provider {provider:?}")]
    DuplicateProvider { provider: AgentId },
    #[error("unavailable provider {provider:?} cannot include version or contract metadata")]
    UnavailableHasMetadata { provider: AgentId },
    #[error("raw passthrough provider {provider:?} cannot include a semantic contract")]
    RawPassthroughHasContract { provider: AgentId },
    #[error("verified semantic provider {provider:?} requires both version and contract metadata")]
    VerifiedSemanticMissingMetadata { provider: AgentId },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    Pty,
    Inline,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnProfileId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnProfileRevision(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnBundleId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnBundleRevision(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnBundleDigest(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnContextId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnContextDigest(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnEnvironmentProfileId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnEnvironmentProfileRevision(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnIdempotencyKey(String);

macro_rules! spawn_identifier_impl {
    ($type:ident, $label:literal, $max:ident) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, SpawnIdentifierError> {
                let value = value.into();
                validate_spawn_identifier($label, &value, $max)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = SpawnIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

spawn_identifier_impl!(SpawnProfileId, "spawn profile", MAX_SPAWN_PROFILE_ID_BYTES);
spawn_identifier_impl!(
    SpawnProfileRevision,
    "spawn profile revision",
    MAX_SPAWN_PROFILE_REVISION_BYTES
);
spawn_identifier_impl!(SpawnBundleId, "spawn bundle", MAX_SPAWN_RESOURCE_ID_BYTES);
spawn_identifier_impl!(
    SpawnBundleRevision,
    "spawn bundle revision",
    MAX_SPAWN_BUNDLE_REVISION_BYTES
);
spawn_identifier_impl!(SpawnContextId, "spawn context", MAX_SPAWN_RESOURCE_ID_BYTES);
spawn_identifier_impl!(
    SpawnEnvironmentProfileId,
    "spawn environment profile",
    MAX_SPAWN_RESOURCE_ID_BYTES
);

impl SpawnBundleDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, SpawnBundleDigestError> {
        let value = value.into();
        let digest = value
            .strip_prefix("sha256:")
            .ok_or(SpawnBundleDigestError)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(SpawnBundleDigestError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SpawnBundleDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SpawnBundleDigest {
    type Err = SpawnBundleDigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for SpawnBundleDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("spawn bundle digest must be sha256: followed by exactly 64 lowercase hexadecimal characters")]
pub struct SpawnBundleDigestError;

impl SpawnContextDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, SpawnContextDigestError> {
        let value = value.into();
        let digest = value
            .strip_prefix("sha256:")
            .ok_or(SpawnContextDigestError)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(SpawnContextDigestError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SpawnContextDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SpawnContextDigest {
    type Err = SpawnContextDigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for SpawnContextDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("spawn context digest must be sha256: followed by exactly 64 lowercase hexadecimal characters")]
pub struct SpawnContextDigestError;
spawn_identifier_impl!(
    SpawnEnvironmentProfileRevision,
    "spawn environment profile revision",
    MAX_SPAWN_ENVIRONMENT_PROFILE_REVISION_BYTES
);
spawn_identifier_impl!(
    SpawnIdempotencyKey,
    "spawn idempotency key",
    MAX_SPAWN_IDEMPOTENCY_KEY_BYTES
);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorktreeProfileId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorktreeProfileRevision(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ManagedWorktreeLeaseId(String);

macro_rules! managed_worktree_identifier_impl {
    ($type:ident, $label:literal, $max:ident) => {
        impl $type {
            pub fn new(
                value: impl Into<String>,
            ) -> Result<Self, ManagedWorktreeIdentifierError> {
                let value = value.into();
                validate_spawn_identifier($label, &value, $max).map_err(
                    |error| ManagedWorktreeIdentifierError { source: error },
                )?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = ManagedWorktreeIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

managed_worktree_identifier_impl!(
    WorktreeProfileId,
    "worktree profile",
    MAX_WORKTREE_PROFILE_ID_BYTES
);
managed_worktree_identifier_impl!(
    WorktreeProfileRevision,
    "worktree profile revision",
    MAX_WORKTREE_PROFILE_REVISION_BYTES
);
managed_worktree_identifier_impl!(
    ManagedWorktreeLeaseId,
    "managed worktree lease",
    MAX_MANAGED_WORKTREE_LEASE_ID_BYTES
);

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid managed worktree identifier: {source}")]
pub struct ManagedWorktreeIdentifierError {
    #[source]
    source: SpawnIdentifierError,
}

fn validate_spawn_identifier(
    label: &'static str,
    value: &str,
    max: usize,
) -> Result<(), SpawnIdentifierError> {
    if value.is_empty() {
        return Err(SpawnIdentifierError::Empty { label });
    }
    if value.len() > max {
        return Err(SpawnIdentifierError::TooLong {
            label,
            len: value.len(),
            max,
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
    {
        return Err(SpawnIdentifierError::InvalidCharacters {
            label,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnIdentifierError {
    #[error("{label} cannot be empty")]
    Empty { label: &'static str },
    #[error("{label} length {len} exceeds the {max}-byte limit")]
    TooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} must contain printable non-whitespace ASCII without path separators: {value}")]
    InvalidCharacters { label: &'static str, value: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SpawnPrompt(String);

impl SpawnPrompt {
    pub fn new(value: impl Into<String>) -> Result<Self, SpawnPromptError> {
        let value = value.into();
        if value.len() > MAX_NODE_TEXT_BYTES {
            return Err(SpawnPromptError::TooLong {
                len: value.len(),
                max: MAX_NODE_TEXT_BYTES,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn byte_len(&self) -> usize {
        self.0.len()
    }
}

impl<'de> Deserialize<'de> for SpawnPrompt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnPromptError {
    #[error("spawn prompt length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SpawnDeadlineMs(u64);

impl SpawnDeadlineMs {
    pub fn new(value: u64) -> Result<Self, SpawnDeadlineError> {
        if value == 0 || value > MAX_SPAWN_DEADLINE_MS {
            return Err(SpawnDeadlineError::OutOfRange {
                value,
                max: MAX_SPAWN_DEADLINE_MS,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for SpawnDeadlineMs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SpawnDeadlineError {
    #[error("spawn deadline {value}ms is outside 1..={max}ms")]
    OutOfRange { value: u64, max: u64 },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SpawnRequiredCapabilities(Vec<CapabilityId>);

impl SpawnRequiredCapabilities {
    pub fn new(
        capabilities: impl IntoIterator<Item = CapabilityId>,
    ) -> Result<Self, SpawnRequiredCapabilitiesError> {
        let mut bounded = Vec::with_capacity(MAX_SPAWN_REQUIRED_CAPABILITIES);
        for capability in capabilities {
            if bounded.len() == MAX_SPAWN_REQUIRED_CAPABILITIES {
                return Err(SpawnRequiredCapabilitiesError::TooMany {
                    max: MAX_SPAWN_REQUIRED_CAPABILITIES,
                });
            }
            if bounded.contains(&capability) {
                return Err(SpawnRequiredCapabilitiesError::Duplicate { capability });
            }
            bounded.push(capability);
        }
        bounded.sort();
        Ok(Self(bounded))
    }

    pub fn as_slice(&self) -> &[CapabilityId] {
        &self.0
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CapabilityId> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for SpawnRequiredCapabilities {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CapabilitiesVisitor;

        impl<'de> Visitor<'de> for CapabilitiesVisitor {
            type Value = SpawnRequiredCapabilities;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {MAX_SPAWN_REQUIRED_CAPABILITIES} unique spawn capabilities",
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut capabilities = Vec::with_capacity(MAX_SPAWN_REQUIRED_CAPABILITIES);
                while let Some(capability) = sequence.next_element::<CapabilityId>()? {
                    if capabilities.len() == MAX_SPAWN_REQUIRED_CAPABILITIES {
                        return Err(serde::de::Error::custom(
                            SpawnRequiredCapabilitiesError::TooMany {
                                max: MAX_SPAWN_REQUIRED_CAPABILITIES,
                            },
                        ));
                    }
                    if capabilities.contains(&capability) {
                        return Err(serde::de::Error::custom(
                            SpawnRequiredCapabilitiesError::Duplicate { capability },
                        ));
                    }
                    capabilities.push(capability);
                }
                capabilities.sort();
                Ok(SpawnRequiredCapabilities(capabilities))
            }
        }

        deserializer.deserialize_seq(CapabilitiesVisitor)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnRequiredCapabilitiesError {
    #[error("spawn required capabilities exceed the {max}-entry limit")]
    TooMany { max: usize },
    #[error("spawn required capabilities contain duplicate {capability}")]
    Duplicate { capability: CapabilityId },
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnTarget {
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SpawnOverride<T> {
    Inherit,
    Set { value: T },
    Clear,
}

impl<T> Default for SpawnOverride<T> {
    fn default() -> Self {
        Self::Inherit
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpawnOverrides {
    pub provider: SpawnOverride<AgentId>,
    pub mode: SpawnOverride<SessionMode>,
    pub terminal_size: SpawnOverride<TerminalSize>,
    pub prompt: SpawnOverride<SpawnPrompt>,
    pub bundle_id: SpawnOverride<SpawnBundleId>,
    pub context_id: SpawnOverride<SpawnContextId>,
    pub environment_profile_id: SpawnOverride<SpawnEnvironmentProfileId>,
}

impl Default for SpawnOverrides {
    fn default() -> Self {
        Self {
            provider: SpawnOverride::Inherit,
            mode: SpawnOverride::Inherit,
            terminal_size: SpawnOverride::Inherit,
            prompt: SpawnOverride::Inherit,
            bundle_id: SpawnOverride::Inherit,
            context_id: SpawnOverride::Inherit,
            environment_profile_id: SpawnOverride::Inherit,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnProfileDefaults {
    pub profile_id: SpawnProfileId,
    pub revision: SpawnProfileRevision,
    pub provider: AgentId,
    pub mode: SessionMode,
    pub terminal_size: TerminalSize,
    pub prompt: Option<SpawnPrompt>,
    pub bundle_id: Option<SpawnBundleId>,
    pub context_id: Option<SpawnContextId>,
    pub environment_profile_id: Option<SpawnEnvironmentProfileId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSpec {
    pub target: SpawnTarget,
    pub profile_id: SpawnProfileId,
    pub expected_profile_revision: SpawnProfileRevision,
    #[serde(default)]
    pub overrides: SpawnOverrides,
    /// Node-local processing budget. It starts when the authenticated Node accepts the
    /// first request for this idempotency key; relay queueing is outside this budget.
    pub deadline_ms: SpawnDeadlineMs,
    pub idempotency_key: SpawnIdempotencyKey,
    #[serde(default)]
    pub required_capabilities: SpawnRequiredCapabilities,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedWorktreeRetention {
    RemoveWhenReleased,
    Retain,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedWorktreeLeaseState {
    Allocating,
    Ready,
    InUse,
    Retained,
    CleanupBlocked,
    RecoveryRequired,
    Removed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedWorktreeCleanupFailure {
    Busy,
    Dirty,
    Locked,
    Prunable,
    OwnershipConflict,
    Backend,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeSpawnRequest {
    pub spawn_spec: SpawnSpec,
    pub worktree_profile_id: WorktreeProfileId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeSpawnRequestV2 {
    pub spawn_spec: SpawnSpec,
    pub worktree_profile_id: WorktreeProfileId,
    pub expected_profile_revision: WorktreeProfileRevision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeLeaseSnapshot {
    pub lease_id: ManagedWorktreeLeaseId,
    pub source_workspace_id: WorkspaceId,
    pub workspace_id: WorkspaceId,
    pub profile_id: WorktreeProfileId,
    pub profile_revision: WorktreeProfileRevision,
    pub retention: ManagedWorktreeRetention,
    pub state: ManagedWorktreeLeaseState,
    pub active_session_count: u16,
    pub managed_record_count: u16,
    pub cleanup_failure: Option<ManagedWorktreeCleanupFailure>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl<'de> Deserialize<'de> for ManagedWorktreeLeaseSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireLease {
            lease_id: ManagedWorktreeLeaseId,
            source_workspace_id: WorkspaceId,
            workspace_id: WorkspaceId,
            profile_id: WorktreeProfileId,
            profile_revision: WorktreeProfileRevision,
            retention: ManagedWorktreeRetention,
            state: ManagedWorktreeLeaseState,
            active_session_count: u16,
            managed_record_count: u16,
            cleanup_failure: Option<ManagedWorktreeCleanupFailure>,
            created_at_unix_ms: u64,
            updated_at_unix_ms: u64,
        }

        let wire = WireLease::deserialize(deserializer)?;
        if wire.source_workspace_id == wire.workspace_id {
            return Err(serde::de::Error::custom(
                "managed worktree workspace cannot alias its source workspace",
            ));
        }
        if wire.updated_at_unix_ms < wire.created_at_unix_ms {
            return Err(serde::de::Error::custom(
                "managed worktree update timestamp precedes creation timestamp",
            ));
        }
        let holder_count = u32::from(wire.active_session_count)
            .saturating_add(u32::from(wire.managed_record_count));
        let valid_state = match wire.state {
            ManagedWorktreeLeaseState::Allocating
            | ManagedWorktreeLeaseState::Ready
            | ManagedWorktreeLeaseState::Retained
            | ManagedWorktreeLeaseState::Removed => {
                holder_count == 0 && wire.cleanup_failure.is_none()
            }
            ManagedWorktreeLeaseState::InUse => {
                holder_count > 0 && wire.cleanup_failure.is_none()
            }
            ManagedWorktreeLeaseState::CleanupBlocked => {
                holder_count == 0 && wire.cleanup_failure.is_some()
            }
            ManagedWorktreeLeaseState::RecoveryRequired => wire.cleanup_failure.is_some(),
        };
        if !valid_state {
            return Err(serde::de::Error::custom(
                "managed worktree state, holder counts, and cleanup failure are inconsistent",
            ));
        }
        Ok(Self {
            lease_id: wire.lease_id,
            source_workspace_id: wire.source_workspace_id,
            workspace_id: wire.workspace_id,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            retention: wire.retention,
            state: wire.state,
            active_session_count: wire.active_session_count,
            managed_record_count: wire.managed_record_count,
            cleanup_failure: wire.cleanup_failure,
            created_at_unix_ms: wire.created_at_unix_ms,
            updated_at_unix_ms: wire.updated_at_unix_ms,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeSpawnReceipt {
    pub spawn: ResolvedSpawnReceipt,
    pub lease: ManagedWorktreeLeaseSnapshot,
}

impl<'de> Deserialize<'de> for ManagedWorktreeSpawnReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReceipt {
            spawn: ResolvedSpawnReceipt,
            lease: ManagedWorktreeLeaseSnapshot,
        }

        let wire = WireReceipt::deserialize(deserializer)?;
        if wire.lease.source_workspace_id != wire.spawn.target.workspace_id
            || wire.spawn.target.worktree_id.as_ref() != Some(&wire.lease.workspace_id)
            || wire.spawn.session.workspace_id != wire.lease.workspace_id
            || wire.lease.state != ManagedWorktreeLeaseState::InUse
            || wire.lease.cleanup_failure.is_some()
            || wire.lease.active_session_count != 1
        {
            return Err(serde::de::Error::custom(
                "managed worktree spawn receipt contains inconsistent lease correlation",
            ));
        }
        Ok(Self {
            spawn: wire.spawn,
            lease: wire.lease,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpawnFieldProvenance {
    Profile,
    Override,
    Cleared,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnResolutionProvenance {
    pub provider: SpawnFieldProvenance,
    pub mode: SpawnFieldProvenance,
    pub terminal_size: SpawnFieldProvenance,
    pub prompt: SpawnFieldProvenance,
    pub bundle_id: SpawnFieldProvenance,
    pub context_id: SpawnFieldProvenance,
    pub environment_profile_id: SpawnFieldProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSpawnSpec {
    pub target: SpawnTarget,
    pub profile_id: SpawnProfileId,
    pub profile_revision: SpawnProfileRevision,
    pub provider: AgentId,
    pub mode: SessionMode,
    pub terminal_size: TerminalSize,
    pub prompt: Option<SpawnPrompt>,
    pub bundle_id: Option<SpawnBundleId>,
    pub context_id: Option<SpawnContextId>,
    pub environment_profile_id: Option<SpawnEnvironmentProfileId>,
    pub deadline_ms: SpawnDeadlineMs,
    pub idempotency_key: SpawnIdempotencyKey,
    pub required_capabilities: SpawnRequiredCapabilities,
    pub provenance: SpawnResolutionProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SpawnPromptMetadata {
    pub present: bool,
    pub byte_len: u32,
}

impl<'de> Deserialize<'de> for SpawnPromptMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireMetadata {
            present: bool,
            byte_len: u32,
        }

        let wire = WireMetadata::deserialize(deserializer)?;
        if usize::try_from(wire.byte_len).unwrap_or(usize::MAX) > MAX_NODE_TEXT_BYTES {
            return Err(serde::de::Error::custom(
                "spawn prompt metadata exceeds the prompt byte limit",
            ));
        }
        if !wire.present && wire.byte_len != 0 {
            return Err(serde::de::Error::custom(
                "absent spawn prompt metadata must report zero bytes",
            ));
        }
        Ok(Self {
            present: wire.present,
            byte_len: wire.byte_len,
        })
    }
}

impl SpawnPromptMetadata {
    pub fn from_prompt(prompt: Option<&SpawnPrompt>) -> Self {
        Self {
            present: prompt.is_some(),
            byte_len: prompt
                .map(SpawnPrompt::byte_len)
                .and_then(|len| u32::try_from(len).ok())
                .unwrap_or(0),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedEnvironmentProfileReceipt {
    pub profile_id: SpawnEnvironmentProfileId,
    pub profile_revision: SpawnEnvironmentProfileRevision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedBundleReceipt {
    pub id: SpawnBundleId,
    pub revision: SpawnBundleRevision,
    pub digest: SpawnBundleDigest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnProfileSummary {
    pub id: SpawnProfileId,
    pub revision: SpawnProfileRevision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeProfileSummary {
    pub id: WorktreeProfileId,
    pub revision: WorktreeProfileRevision,
    pub retention: ManagedWorktreeRetention,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorktreeProfileInventory {
    pub profiles: Vec<ManagedWorktreeProfileSummary>,
}

impl<'de> Deserialize<'de> for WorktreeProfileInventory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_bounded_worktree_profiles(deserializer).map(|profiles| Self { profiles })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchInventory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_profiles: Option<Vec<SpawnProfileSummary>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundles: Option<Vec<ResolvedBundleReceipt>>,
}

impl<'de> Deserialize<'de> for LaunchInventory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireInventory {
            #[serde(default)]
            spawn_profiles: Option<BoundedSpawnProfiles>,
            #[serde(default)]
            bundles: Option<BoundedLaunchBundles>,
        }

        let wire = WireInventory::deserialize(deserializer)?;
        if wire.spawn_profiles.is_none() && wire.bundles.is_none() {
            return Err(serde::de::Error::custom(
                "launch inventory must expose at least one negotiated component",
            ));
        }
        Ok(Self {
            spawn_profiles: wire.spawn_profiles.map(|profiles| profiles.0),
            bundles: wire.bundles.map(|bundles| bundles.0),
        })
    }
}

struct BoundedSpawnProfiles(Vec<SpawnProfileSummary>);

impl<'de> Deserialize<'de> for BoundedSpawnProfiles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SpawnProfilesVisitor;

        impl<'de> Visitor<'de> for SpawnProfilesVisitor {
            type Value = BoundedSpawnProfiles;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "at most {MAX_SPAWN_PROFILES} spawn profiles")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut profiles = Vec::with_capacity(
                    sequence.size_hint().unwrap_or(0).min(MAX_SPAWN_PROFILES),
                );
                while let Some(profile) = sequence.next_element::<SpawnProfileSummary>()? {
                    if profiles.len() == MAX_SPAWN_PROFILES {
                        return Err(serde::de::Error::invalid_length(profiles.len() + 1, &self));
                    }
                    if profiles.iter().any(|existing: &SpawnProfileSummary| existing.id == profile.id) {
                        return Err(serde::de::Error::custom(
                            "launch inventory contains duplicate spawn profile identity",
                        ));
                    }
                    profiles.push(profile);
                }
                Ok(BoundedSpawnProfiles(profiles))
            }
        }

        deserializer.deserialize_seq(SpawnProfilesVisitor)
    }
}

struct BoundedLaunchBundles(Vec<ResolvedBundleReceipt>);

impl<'de> Deserialize<'de> for BoundedLaunchBundles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LaunchBundlesVisitor;

        impl<'de> Visitor<'de> for LaunchBundlesVisitor {
            type Value = BoundedLaunchBundles;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "at most {MAX_LAUNCH_BUNDLES} launch bundles")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bundles = Vec::with_capacity(
                    sequence.size_hint().unwrap_or(0).min(MAX_LAUNCH_BUNDLES),
                );
                while let Some(bundle) = sequence.next_element::<ResolvedBundleReceipt>()? {
                    if bundles.len() == MAX_LAUNCH_BUNDLES {
                        return Err(serde::de::Error::invalid_length(bundles.len() + 1, &self));
                    }
                    if bundles.iter().any(|existing: &ResolvedBundleReceipt| existing.id == bundle.id) {
                        return Err(serde::de::Error::custom(
                            "launch inventory contains duplicate bundle identity",
                        ));
                    }
                    bundles.push(bundle);
                }
                Ok(BoundedLaunchBundles(bundles))
            }
        }

        deserializer.deserialize_seq(LaunchBundlesVisitor)
    }
}

fn deserialize_bounded_worktree_profiles<'de, D>(
    deserializer: D,
) -> Result<Vec<ManagedWorktreeProfileSummary>, D::Error>
where
    D: Deserializer<'de>,
{
    struct WorktreeProfilesVisitor;

    impl<'de> Visitor<'de> for WorktreeProfilesVisitor {
        type Value = Vec<ManagedWorktreeProfileSummary>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE} managed worktree profiles",
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut profiles = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE),
            );
            while let Some(profile) = sequence.next_element::<ManagedWorktreeProfileSummary>()? {
                if profiles.len() == MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE {
                    return Err(serde::de::Error::invalid_length(profiles.len() + 1, &self));
                }
                if profiles.iter().any(|existing: &ManagedWorktreeProfileSummary| existing.id == profile.id) {
                    return Err(serde::de::Error::custom(
                        "workspace inventory contains duplicate managed worktree profile identity",
                    ));
                }
                profiles.push(profile);
            }
            Ok(profiles)
        }
    }

    deserializer.deserialize_seq(WorktreeProfilesVisitor)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPackLineageReceipt {
    pub source_node_id: NodeId,
    pub source_session: SessionAddress,
    pub source_provider: AgentId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedContextPackReceipt {
    pub id: SpawnContextId,
    pub digest: SpawnContextDigest,
    pub lineage: ContextPackLineageReceipt,
    pub source_message_count: u64,
    pub retained_message_count: u64,
    pub byte_len: u32,
    pub truncated: bool,
}

impl<'de> Deserialize<'de> for ResolvedContextPackReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReceipt {
            id: SpawnContextId,
            digest: SpawnContextDigest,
            lineage: ContextPackLineageReceipt,
            source_message_count: u64,
            retained_message_count: u64,
            byte_len: u32,
            truncated: bool,
        }

        let wire = WireReceipt::deserialize(deserializer)?;
        let receipt = Self {
            id: wire.id,
            digest: wire.digest,
            lineage: wire.lineage,
            source_message_count: wire.source_message_count,
            retained_message_count: wire.retained_message_count,
            byte_len: wire.byte_len,
            truncated: wire.truncated,
        };
        if !receipt.is_valid() {
            return Err(serde::de::Error::custom(
                "context pack receipt is empty, inconsistent, or exceeds protocol limits",
            ));
        }
        Ok(receipt)
    }
}

impl ResolvedContextPackReceipt {
    pub fn is_valid(&self) -> bool {
        self.source_message_count > 0
            && self.retained_message_count > 0
            && self.retained_message_count <= self.source_message_count
            && self.retained_message_count <= MAX_CONTEXT_PACK_RETAINED_MESSAGES
            && self.byte_len > 0
            && self.byte_len <= MAX_CONTEXT_PACK_BYTES
            && self.truncated
                == (self.source_message_count > self.retained_message_count)
    }
}

fn context_receipt_binding_is_valid(
    context_id: Option<&SpawnContextId>,
    context: Option<&ResolvedContextPackReceipt>,
) -> bool {
    match (context_id, context) {
        (None, None) => true,
        (Some(context_id), Some(context)) => &context.id == context_id && context.is_valid(),
        (None, Some(_)) | (Some(_), None) => false,
    }
}

#[derive(Debug, Error)]
pub enum HarnessMcpContractError {
    #[error("invalid harness MCP reservation ID")]
    InvalidReservationId,
    #[error("invalid harness MCP call ID")]
    InvalidCallId,
    #[error("invalid harness MCP activation digest")]
    InvalidActivationDigest,
    #[error("invalid harness MCP local token")]
    InvalidLocalToken,
    #[error("invalid harness MCP reply chunk")]
    InvalidReplyChunk,
    #[error("invalid harness MCP local request")]
    InvalidLocalRequest,
    #[error("invalid harness MCP local reply")]
    InvalidLocalReply,
}

fn is_exact_lower_hex(value: &str, prefix: &str, digits: usize) -> bool {
    value.len() == prefix.len() + digits
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

macro_rules! harness_mcp_wire_string {
    ($name:ident, $prefix:literal, $digits:literal, $error:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, HarnessMcpContractError> {
                let value = value.into();
                if !is_exact_lower_hex(&value, $prefix, $digits) {
                    return Err(HarnessMcpContractError::$error);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: Deserializer<'de> {
                Self::new(String::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

harness_mcp_wire_string!(HarnessMcpReservationId, "hmcpres_", 24, InvalidReservationId);
harness_mcp_wire_string!(HarnessMcpCallId, "hmcpcall_", 24, InvalidCallId);
harness_mcp_wire_string!(HarnessMcpActivationDigest, "sha256:", 64, InvalidActivationDigest);

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessMcpLocalToken(String);

impl HarnessMcpLocalToken {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessMcpContractError> {
        let value = value.into();
        if !is_exact_lower_hex(&value, "g4ah3_", 64) {
            return Err(HarnessMcpContractError::InvalidLocalToken);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for HarnessMcpLocalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HarnessMcpLocalToken([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for HarnessMcpLocalToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMcpLocalRequestV1 {
    pub version: u16,
    pub token: HarnessMcpLocalToken,
    pub request: HarnessReadRequestV1,
}

impl HarnessMcpLocalRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessMcpContractError> {
        if self.version != 1
            || self.request.validate().is_err()
            || serde_json::to_vec(self)
                .map_or(true, |wire| wire.len() > MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES)
        {
            return Err(HarnessMcpContractError::InvalidLocalRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessMcpLocalReplyV1 {
    Ok { response: HarnessReadResponseV1 },
    Error { error: HarnessReadHostErrorV1 },
}

impl HarnessMcpLocalReplyV1 {
    pub fn validate(&self) -> Result<(), HarnessMcpContractError> {
        let content_is_valid = match self {
            Self::Ok { response } => response.validate().is_ok(),
            Self::Error { .. } => true,
        };
        if !content_is_valid
            || serde_json::to_vec(self)
                .map_or(true, |wire| wire.len() > MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES)
        {
            return Err(HarnessMcpContractError::InvalidLocalReply);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct HarnessMcpReplyChunkHexV1(String);

impl HarnessMcpReplyChunkHexV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessMcpContractError> {
        let value = value.into();
        if value.len() > MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES * 2
            || value.len() % 2 != 0
            || !value.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            })
        {
            return Err(HarnessMcpContractError::InvalidReplyChunk);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }
    pub fn raw_len(&self) -> usize { self.0.len() / 2 }
}

impl<'de> Deserialize<'de> for HarnessMcpReplyChunkHexV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessMcpRejectReasonV1 {
    Unauthorized,
    Unavailable,
    InvalidRequest,
    NotFoundOrDenied,
    ResponseTooLarge,
    Deadline,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedHarnessMcpProxyReceiptV1 {
    pub reservation_id: HarnessMcpReservationId,
    pub activation_digest: HarnessMcpActivationDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSpawnReceipt {
    pub incarnation_id: NodeIncarnationId,
    pub session: SessionAddress,
    pub target: SpawnTarget,
    pub profile_id: SpawnProfileId,
    pub profile_revision: SpawnProfileRevision,
    pub provider: AgentId,
    pub mode: SessionMode,
    pub terminal_size: TerminalSize,
    pub prompt: SpawnPromptMetadata,
    pub bundle_id: Option<SpawnBundleId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<ResolvedBundleReceipt>,
    pub context_id: Option<SpawnContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ResolvedContextPackReceipt>,
    #[serde(rename = "environment_profile_id")]
    pub environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    pub deadline_ms: SpawnDeadlineMs,
    pub idempotency_key: SpawnIdempotencyKey,
    pub required_capabilities: SpawnRequiredCapabilities,
    pub provenance: SpawnResolutionProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_mcp_proxy: Option<ResolvedHarnessMcpProxyReceiptV1>,
}

impl ResolvedSpawnReceipt {
    pub fn context_binding_is_valid(&self) -> bool {
        context_receipt_binding_is_valid(self.context_id.as_ref(), self.context.as_ref())
    }
}

impl<'de> Deserialize<'de> for ResolvedSpawnReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireReceipt {
            incarnation_id: NodeIncarnationId,
            session: SessionAddress,
            target: SpawnTarget,
            profile_id: SpawnProfileId,
            profile_revision: SpawnProfileRevision,
            provider: AgentId,
            mode: SessionMode,
            terminal_size: TerminalSize,
            prompt: SpawnPromptMetadata,
            bundle_id: Option<SpawnBundleId>,
            #[serde(default)]
            bundle: Option<ResolvedBundleReceipt>,
            context_id: Option<SpawnContextId>,
            #[serde(default)]
            context: Option<ResolvedContextPackReceipt>,
            #[serde(rename = "environment_profile_id")]
            environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
            deadline_ms: SpawnDeadlineMs,
            idempotency_key: SpawnIdempotencyKey,
            required_capabilities: SpawnRequiredCapabilities,
            provenance: SpawnResolutionProvenance,
            #[serde(default)]
            harness_mcp_proxy: Option<ResolvedHarnessMcpProxyReceiptV1>,
        }

        let wire = WireReceipt::deserialize(deserializer)?;
        let receipt = Self {
            incarnation_id: wire.incarnation_id,
            session: wire.session,
            target: wire.target,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            provider: wire.provider,
            mode: wire.mode,
            terminal_size: wire.terminal_size,
            prompt: wire.prompt,
            bundle_id: wire.bundle_id,
            bundle: wire.bundle,
            context_id: wire.context_id,
            context: wire.context,
            environment_profile: wire.environment_profile,
            deadline_ms: wire.deadline_ms,
            idempotency_key: wire.idempotency_key,
            required_capabilities: wire.required_capabilities,
            provenance: wire.provenance,
            harness_mcp_proxy: wire.harness_mcp_proxy,
        };
        if !receipt.context_binding_is_valid() {
            return Err(serde::de::Error::custom(
                "spawn receipt context id and materialization receipt are not correlated",
            ));
        }
        Ok(receipt)
    }
}

impl SpawnSpec {
    pub fn resolve(
        &self,
        defaults: &SpawnProfileDefaults,
    ) -> Result<ResolvedSpawnSpec, SpawnSpecResolveError> {
        if self.profile_id != defaults.profile_id {
            return Err(SpawnSpecResolveError::ProfileMismatch {
                requested: self.profile_id.clone(),
                loaded: defaults.profile_id.clone(),
            });
        }
        if self.expected_profile_revision != defaults.revision {
            return Err(SpawnSpecResolveError::ProfileRevisionMismatch {
                expected: self.expected_profile_revision.clone(),
                loaded: defaults.revision.clone(),
            });
        }
        let (provider, provider_source) = resolve_required_spawn_field(
            "provider",
            &defaults.provider,
            &self.overrides.provider,
        )?;
        let (mode, mode_source) = resolve_required_spawn_field(
            "mode",
            &defaults.mode,
            &self.overrides.mode,
        )?;
        let (terminal_size, terminal_size_source) = resolve_required_spawn_field(
            "terminal_size",
            &defaults.terminal_size,
            &self.overrides.terminal_size,
        )?;
        if !terminal_size.is_valid() {
            return Err(SpawnSpecResolveError::InvalidTerminalSize);
        }
        let (prompt, prompt_source) = resolve_optional_spawn_field(
            &defaults.prompt,
            &self.overrides.prompt,
        );
        let (bundle_id, bundle_source) = resolve_optional_spawn_field(
            &defaults.bundle_id,
            &self.overrides.bundle_id,
        );
        let (context_id, context_source) = resolve_optional_spawn_field(
            &defaults.context_id,
            &self.overrides.context_id,
        );
        let (environment_profile_id, environment_profile_source) =
            resolve_optional_spawn_field(
                &defaults.environment_profile_id,
                &self.overrides.environment_profile_id,
            );
        Ok(ResolvedSpawnSpec {
            target: self.target.clone(),
            profile_id: self.profile_id.clone(),
            profile_revision: self.expected_profile_revision.clone(),
            provider,
            mode,
            terminal_size,
            prompt,
            bundle_id,
            context_id,
            environment_profile_id,
            deadline_ms: self.deadline_ms,
            idempotency_key: self.idempotency_key.clone(),
            required_capabilities: self.required_capabilities.clone(),
            provenance: SpawnResolutionProvenance {
                provider: provider_source,
                mode: mode_source,
                terminal_size: terminal_size_source,
                prompt: prompt_source,
                bundle_id: bundle_source,
                context_id: context_source,
                environment_profile_id: environment_profile_source,
            },
        })
    }
}

impl ResolvedSpawnSpec {
    pub fn receipt(
        &self,
        incarnation_id: NodeIncarnationId,
        session: SessionAddress,
    ) -> ResolvedSpawnReceipt {
        self.receipt_with_materialization(incarnation_id, session, None, None, None)
    }

    pub fn receipt_with_environment(
        &self,
        incarnation_id: NodeIncarnationId,
        session: SessionAddress,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    ) -> ResolvedSpawnReceipt {
        self.receipt_with_materialization(
            incarnation_id,
            session,
            environment_profile,
            None,
            None,
        )
    }

    pub fn receipt_with_materialization(
        &self,
        incarnation_id: NodeIncarnationId,
        session: SessionAddress,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
    ) -> ResolvedSpawnReceipt {
        ResolvedSpawnReceipt {
            incarnation_id,
            session,
            target: self.target.clone(),
            profile_id: self.profile_id.clone(),
            profile_revision: self.profile_revision.clone(),
            provider: self.provider.clone(),
            mode: self.mode,
            terminal_size: self.terminal_size,
            prompt: SpawnPromptMetadata::from_prompt(self.prompt.as_ref()),
            bundle_id: self.bundle_id.clone(),
            bundle,
            context_id: self.context_id.clone(),
            context,
            environment_profile,
            deadline_ms: self.deadline_ms,
            idempotency_key: self.idempotency_key.clone(),
            required_capabilities: self.required_capabilities.clone(),
            provenance: self.provenance.clone(),
            harness_mcp_proxy: None,
        }
    }
}

fn resolve_required_spawn_field<T: Clone>(
    field: &'static str,
    default: &T,
    override_value: &SpawnOverride<T>,
) -> Result<(T, SpawnFieldProvenance), SpawnSpecResolveError> {
    match override_value {
        SpawnOverride::Inherit => Ok((default.clone(), SpawnFieldProvenance::Profile)),
        SpawnOverride::Set { value } => Ok((value.clone(), SpawnFieldProvenance::Override)),
        SpawnOverride::Clear => Err(SpawnSpecResolveError::RequiredFieldCleared { field }),
    }
}

fn resolve_optional_spawn_field<T: Clone>(
    default: &Option<T>,
    override_value: &SpawnOverride<T>,
) -> (Option<T>, SpawnFieldProvenance) {
    match override_value {
        SpawnOverride::Inherit => (default.clone(), SpawnFieldProvenance::Profile),
        SpawnOverride::Set { value } => {
            (Some(value.clone()), SpawnFieldProvenance::Override)
        }
        SpawnOverride::Clear => (None, SpawnFieldProvenance::Cleared),
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnSpecResolveError {
    #[error("spawn profile {requested} does not match loaded profile {loaded}")]
    ProfileMismatch {
        requested: SpawnProfileId,
        loaded: SpawnProfileId,
    },
    #[error("expected spawn profile revision {expected} does not match loaded revision {loaded}")]
    ProfileRevisionMismatch {
        expected: SpawnProfileRevision,
        loaded: SpawnProfileRevision,
    },
    #[error("required spawn field {field} cannot be cleared")]
    RequiredFieldCleared { field: &'static str },
    #[error("resolved spawn terminal size is invalid")]
    InvalidTerminalSize,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionRecordId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, TaskIdError> {
        let value = value.into();
        let hex = value.strip_prefix("task-").ok_or(TaskIdError)?;
        if hex.len() != TASK_ID_NONCE_BYTES * 2
            || !hex.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(TaskIdError);
        }
        Ok(Self(value))
    }

    pub fn from_nonce(nonce: [u8; TASK_ID_NONCE_BYTES]) -> Self {
        let mut value = String::with_capacity(5 + TASK_ID_NONCE_BYTES * 2);
        value.push_str("task-");
        for byte in nonce {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for TaskId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for TaskId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TaskId {
    type Err = TaskIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("task ID must be exactly `task-` followed by 24 lowercase hexadecimal characters")]
pub struct TaskIdError;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeIncarnationId([u8; NODE_INCARNATION_ID_BYTES]);

impl NodeIncarnationId {
    pub fn from_bytes(bytes: [u8; NODE_INCARNATION_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; NODE_INCARNATION_ID_BYTES] {
        &self.0
    }
}

impl fmt::Display for NodeIncarnationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for NodeIncarnationId {
    type Err = NodeIncarnationIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != NODE_INCARNATION_ID_BYTES * 2 {
            return Err(NodeIncarnationIdError::InvalidLength {
                len: value.len(),
                expected: NODE_INCARNATION_ID_BYTES * 2,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) {
            return Err(NodeIncarnationIdError::InvalidHex(value.to_owned()));
        }
        let mut bytes = [0; NODE_INCARNATION_ID_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (decode_lower_hex(pair[0]) << 4) | decode_lower_hex(pair[1]);
        }
        Ok(Self(bytes))
    }
}

impl Serialize for NodeIncarnationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodeIncarnationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn decode_lower_hex(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("lowercase hexadecimal input was validated"),
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NodeIncarnationIdError {
    #[error("node incarnation ID length {len} does not match the required {expected} lowercase hexadecimal characters")]
    InvalidLength { len: usize, expected: usize },
    #[error("node incarnation ID must contain exactly 32 lowercase hexadecimal characters: {0}")]
    InvalidHex(String),
}

macro_rules! identifier_impl {
    ($type:ident, $label:literal) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, NodeIdentifierError> {
                let value = value.into();
                validate_identifier($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $type {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl Borrow<str> for $type {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = NodeIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier_impl!(NodeId, "node");
identifier_impl!(WorkspaceId, "workspace");
identifier_impl!(SessionRecordId, "session record");

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionCatalogRoute {
    pub scope: NativeSessionCatalogScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub provider: AgentId,
}

impl NativeSessionCatalogRoute {
    pub fn workspace(workspace_id: WorkspaceId, provider: AgentId) -> Self {
        Self {
            scope: NativeSessionCatalogScope::Workspace,
            workspace_id: Some(workspace_id),
            provider,
        }
    }

    pub fn unregistered(provider: AgentId) -> Self {
        Self {
            scope: NativeSessionCatalogScope::Unregistered,
            workspace_id: None,
            provider,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match (self.scope, self.workspace_id.as_ref()) {
            (NativeSessionCatalogScope::Workspace, Some(_))
            | (NativeSessionCatalogScope::Unregistered, None) => Ok(()),
            _ => Err("native session catalog route scope and workspace do not match"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionSelection {
    pub route: NativeSessionCatalogRoute,
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    #[serde(deserialize_with = "deserialize_history_candidate_id")]
    pub selection_id: String,
}

impl NativeSessionSelection {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.route.validate()?;
        if self.catalog_revision == 0 {
            return Err("native session selection catalog revision is invalid");
        }
        gate4agent_types::validate_candidate_id(&self.selection_id)
            .map_err(|_| "native session selection ID is invalid")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionCatalogEntry {
    pub selection_id: String,
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub completed_turn_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_group: Option<NativeSessionExternalGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<SessionRecordId>,
}

impl NativeSessionCatalogEntry {
    pub fn validate(&self) -> Result<(), gate4agent_types::HistoryValidationError> {
        gate4agent_types::NativeSessionCatalogEntry {
            selection_id: self.selection_id.clone(),
            session_id: "redacted-provider-session".to_owned(),
            title: self.title.clone(),
            modified_at_unix_ms: self.modified_at_unix_ms,
            model: self.model.clone(),
            message_count: self.message_count,
            completed_turn_count: self.completed_turn_count,
        }
        .validate()?;
        if let Some(group) = self.external_group.as_ref() {
            group.validate()?;
        }
        Ok(())
    }

    pub fn validate_for_route(
        &self,
        route: &NativeSessionCatalogRoute,
    ) -> Result<(), &'static str> {
        self.validate()
            .map_err(|_| "native session catalog entry is invalid")?;
        match route.scope {
            NativeSessionCatalogScope::Workspace if self.external_group.is_none() => Ok(()),
            NativeSessionCatalogScope::Unregistered
                if self.external_group.is_some() && self.record_id.is_none() => Ok(()),
            NativeSessionCatalogScope::Workspace => {
                Err("workspace native session entry contains an external group")
            }
            NativeSessionCatalogScope::Unregistered => {
                Err("unregistered native session entry is missing its group or exposes a record")
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionCatalogPage {
    pub window: NativeSessionCatalogWindow,
    pub revision: u64,
    pub entries: Vec<NativeSessionCatalogEntry>,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
    pub has_more: bool,
}

impl NativeSessionCatalogPage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.entries.len() > usize::from(NATIVE_SESSION_CATALOG_LIMIT_MAX) {
            return Err("native session catalog page exceeds the bounded entry limit");
        }
        for (index, entry) in self.entries.iter().enumerate() {
            entry
                .validate()
                .map_err(|_| "native session catalog entry is invalid")?;
            if self.entries[..index]
                .iter()
                .any(|existing| existing.selection_id == entry.selection_id)
            {
                return Err("native session catalog page contains duplicate selections");
            }
            if entry.record_id.as_ref().is_some_and(|record_id| {
                self.entries[..index]
                    .iter()
                    .any(|existing| existing.record_id.as_ref() == Some(record_id))
            }) {
                return Err("native session catalog page contains duplicate managed records");
            }
        }
        if self
            .next_after_selection_id
            .as_deref()
            .is_some_and(|cursor| gate4agent_types::validate_candidate_id(cursor).is_err())
            || self.has_more != self.next_after_selection_id.is_some()
            || self.has_more != (self.remaining_count > 0)
        {
            return Err("native session catalog page cursor is invalid");
        }
        Ok(())
    }

    pub fn validate_for_route(
        &self,
        route: &NativeSessionCatalogRoute,
    ) -> Result<(), &'static str> {
        route.validate()?;
        self.validate()?;
        for entry in &self.entries {
            entry.validate_for_route(route)?;
        }
        Ok(())
    }
}

pub type NativeSessionPreview = SessionRecordPreview;

fn validate_identifier(label: &'static str, value: &str) -> Result<(), NodeIdentifierError> {
    if value.is_empty() {
        return Err(NodeIdentifierError::Empty { label });
    }
    if value.len() > MAX_NODE_IDENTIFIER_BYTES {
        return Err(NodeIdentifierError::TooLong {
            label,
            len: value.len(),
            max: MAX_NODE_IDENTIFIER_BYTES,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(NodeIdentifierError::InvalidCharacters {
            label,
            value: value.to_owned(),
        });
    }
    if matches!(value.as_bytes().first(), Some(b'-' | b'_'))
        || matches!(value.as_bytes().last(), Some(b'-' | b'_'))
    {
        return Err(NodeIdentifierError::InvalidBoundary {
            label,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NodeIdentifierError {
    #[error("{label} ID cannot be empty")]
    Empty { label: &'static str },
    #[error("{label} ID length {len} exceeds the {max}-byte limit")]
    TooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} ID must contain only lowercase ASCII letters, digits, '-' or '_': {value}")]
    InvalidCharacters { label: &'static str, value: String },
    #[error("{label} ID cannot start or end with '-' or '_': {value}")]
    InvalidBoundary { label: &'static str, value: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProtocolRange {
    minimum: u16,
    maximum: u16,
}

impl ProtocolRange {
    pub fn new(minimum: u16, maximum: u16) -> Result<Self, ProtocolNegotiationError> {
        if minimum == 0 || maximum == 0 || minimum > maximum {
            return Err(ProtocolNegotiationError::InvalidRange { minimum, maximum });
        }
        Ok(Self { minimum, maximum })
    }

    pub fn exact(version: u16) -> Result<Self, ProtocolNegotiationError> {
        Self::new(version, version)
    }

    pub fn minimum(self) -> u16 {
        self.minimum
    }

    pub fn maximum(self) -> u16 {
        self.maximum
    }

    pub fn contains(self, version: u16) -> bool {
        self.minimum <= version && version <= self.maximum
    }

    pub fn highest_common(self, other: Self) -> Result<u16, ProtocolNegotiationError> {
        let minimum = self.minimum.max(other.minimum);
        let maximum = self.maximum.min(other.maximum);
        if minimum > maximum {
            return Err(ProtocolNegotiationError::Disjoint {
                local: self,
                remote: other,
            });
        }
        Ok(maximum)
    }
}

impl<'de> Deserialize<'de> for ProtocolRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRange {
            minimum: u16,
            maximum: u16,
        }

        let wire = WireRange::deserialize(deserializer)?;
        Self::new(wire.minimum, wire.maximum).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProtocolNegotiationError {
    #[error("protocol range {minimum}..={maximum} is invalid")]
    InvalidRange { minimum: u16, maximum: u16 },
    #[error("protocol ranges {local:?} and {remote:?} do not overlap")]
    Disjoint {
        local: ProtocolRange,
        remote: ProtocolRange,
    },
    #[error("active wire protocol {active} is not contained in both ranges {local:?} and {remote:?}")]
    ActiveVersionUnsupported {
        active: u16,
        local: ProtocolRange,
        remote: ProtocolRange,
    },
    #[error("provider contract manifest is invalid: {0}")]
    InvalidProviderContractManifest(ProviderContractManifestError),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperatingSystemId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArchitectureId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderContractRevision(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdapterContractRevision(String);

macro_rules! compatibility_identifier_impl {
    ($type:ident, $label:literal) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
                let value = value.into();
                validate_compatibility_identifier($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = CompatibilityIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

compatibility_identifier_impl!(CapabilityId, "capability");
compatibility_identifier_impl!(OperatingSystemId, "operating system");
compatibility_identifier_impl!(ArchitectureId, "architecture");
compatibility_identifier_impl!(ProviderRuntimeContractId, "provider runtime contract");

impl ProviderRuntimeVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CompatibilityIdentifierError::Empty {
                label: "provider runtime version",
            });
        }
        if value.len() > MAX_PROVIDER_RUNTIME_VERSION_BYTES {
            return Err(CompatibilityIdentifierError::TooLong {
                label: "provider runtime version",
                len: value.len(),
                max: MAX_PROVIDER_RUNTIME_VERSION_BYTES,
            });
        }
        let mut components = value.split('.');
        let valid_component = |component: &str| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (component.len() == 1 || !component.starts_with('0'))
                && component.parse::<u64>().is_ok()
        };
        if !components.by_ref().take(3).all(valid_component) || components.next().is_some() {
            return Err(CompatibilityIdentifierError::InvalidCharacters {
                label: "provider runtime version",
                value,
            });
        }
        let component_count = value.bytes().filter(|byte| *byte == b'.').count() + 1;
        if component_count != 3 {
            return Err(CompatibilityIdentifierError::InvalidCharacters {
                label: "provider runtime version",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderRuntimeVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderRuntimeVersion {
    type Err = CompatibilityIdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ProviderRuntimeVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderRuntimeVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl ProviderContractRevision {
    pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CompatibilityIdentifierError::Empty {
                label: "provider contract revision",
            });
        }
        if value.len() > MAX_PROVIDER_CONTRACT_REVISION_BYTES {
            return Err(CompatibilityIdentifierError::TooLong {
                label: "provider contract revision",
                len: value.len(),
                max: MAX_PROVIDER_CONTRACT_REVISION_BYTES,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CompatibilityIdentifierError::InvalidCharacters {
                label: "provider contract revision",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderContractRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderContractRevision {
    type Err = CompatibilityIdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ProviderContractRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderContractRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl AdapterContractRevision {
    pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CompatibilityIdentifierError::Empty {
                label: "adapter contract revision",
            });
        }
        if value.len() > MAX_ADAPTER_CONTRACT_REVISION_BYTES {
            return Err(CompatibilityIdentifierError::TooLong {
                label: "adapter contract revision",
                len: value.len(),
                max: MAX_ADAPTER_CONTRACT_REVISION_BYTES,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CompatibilityIdentifierError::InvalidCharacters {
                label: "adapter contract revision",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AdapterContractRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AdapterContractRevision {
    type Err = CompatibilityIdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for AdapterContractRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AdapterContractRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_compatibility_identifier(
    label: &'static str,
    value: &str,
) -> Result<(), CompatibilityIdentifierError> {
    if value.is_empty() {
        return Err(CompatibilityIdentifierError::Empty { label });
    }
    if value.len() > MAX_COMPATIBILITY_IDENTIFIER_BYTES {
        return Err(CompatibilityIdentifierError::TooLong {
            label,
            len: value.len(),
            max: MAX_COMPATIBILITY_IDENTIFIER_BYTES,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_' | b'.')
    }) || !value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || !value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(CompatibilityIdentifierError::InvalidCharacters {
            label,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CompatibilityIdentifierError {
    #[error("{label} identifier cannot be empty")]
    Empty { label: &'static str },
    #[error("{label} identifier length {len} exceeds the {max}-byte limit")]
    TooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} identifier must be bounded lowercase ASCII: {value}")]
    InvalidCharacters { label: &'static str, value: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathStyle {
    Windows,
    Posix,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathEncoding {
    Utf8,
    UnixBytes,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OpaqueHostPath(OpaqueHostPathRepr);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum OpaqueHostPathRepr {
    Utf8(String),
    UnixBytes(Vec<u8>),
}

impl OpaqueHostPath {
    pub fn utf8(value: String) -> Result<Self, OpaqueHostPathError> {
        validate_opaque_host_path(&value.as_bytes())?;
        Ok(Self(OpaqueHostPathRepr::Utf8(value)))
    }

    pub fn unix_bytes(value: Vec<u8>) -> Result<Self, OpaqueHostPathError> {
        validate_opaque_host_path(&value)?;
        Ok(Self(OpaqueHostPathRepr::UnixBytes(value)))
    }

    pub fn byte_len(&self) -> usize {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => value.len(),
            OpaqueHostPathRepr::UnixBytes(value) => value.len(),
        }
    }

    pub fn display_text(&self) -> Cow<'_, str> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => Cow::Borrowed(value),
            OpaqueHostPathRepr::UnixBytes(value) => String::from_utf8_lossy(value),
        }
    }

    pub fn as_utf8(&self) -> Option<&str> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => Some(value),
            OpaqueHostPathRepr::UnixBytes(_) => None,
        }
    }

    pub fn as_unix_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(_) => None,
            OpaqueHostPathRepr::UnixBytes(value) => Some(value),
        }
    }
}

fn validate_opaque_host_path(value: &[u8]) -> Result<(), OpaqueHostPathError> {
    if value.is_empty() {
        return Err(OpaqueHostPathError::Empty);
    }
    if value.len() > MAX_WORKSPACE_ROOT_BYTES {
        return Err(OpaqueHostPathError::TooLong {
            len: value.len(),
            max: MAX_WORKSPACE_ROOT_BYTES,
        });
    }
    if value.contains(&0) {
        return Err(OpaqueHostPathError::ContainsNul);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OpaqueHostPathError {
    #[error("host path cannot be empty")]
    Empty,
    #[error("host path length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("host path cannot contain a NUL byte")]
    ContainsNul,
}

impl Serialize for OpaqueHostPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => serializer.serialize_str(value),
            OpaqueHostPathRepr::UnixBytes(value) => {
                let mut state = serializer.serialize_struct("OpaqueHostPath", 2)?;
                state.serialize_field("kind", "unix-bytes")?;
                state.serialize_field("bytes", value)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for OpaqueHostPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OpaqueHostPathVisitor)
    }
}

struct OpaqueHostPathVisitor;

impl<'de> Visitor<'de> for OpaqueHostPathVisitor {
    type Value = OpaqueHostPath;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded UTF-8 path string or strict unix-bytes path object")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OpaqueHostPath::utf8(value.to_owned()).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OpaqueHostPath::utf8(value).map_err(E::custom)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut kind = None;
        let mut bytes = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(serde::de::Error::duplicate_field("kind"));
                    }
                    kind = Some(map.next_value::<String>()?);
                }
                "bytes" => {
                    if bytes.is_some() {
                        return Err(serde::de::Error::duplicate_field("bytes"));
                    }
                    bytes = Some(map.next_value::<BoundedOpaquePathBytes>()?.0);
                }
                _ => {
                    return Err(serde::de::Error::unknown_field(&field, &["kind", "bytes"]));
                }
            }
        }
        let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
        if kind != "unix-bytes" {
            return Err(serde::de::Error::unknown_variant(&kind, &["unix-bytes"]));
        }
        let bytes = bytes.ok_or_else(|| serde::de::Error::missing_field("bytes"))?;
        OpaqueHostPath::unix_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

struct BoundedOpaquePathBytes(Vec<u8>);

impl<'de> Deserialize<'de> for BoundedOpaquePathBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedOpaquePathBytesVisitor)
    }
}

struct BoundedOpaquePathBytesVisitor;

impl<'de> Visitor<'de> for BoundedOpaquePathBytesVisitor {
    type Value = BoundedOpaquePathBytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at most {MAX_WORKSPACE_ROOT_BYTES} path bytes")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(
            sequence.size_hint().unwrap_or(0).min(MAX_WORKSPACE_ROOT_BYTES),
        );
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == MAX_WORKSPACE_ROOT_BYTES {
                return Err(serde::de::Error::invalid_length(
                    bytes.len() + 1,
                    &self,
                ));
            }
            bytes.push(byte);
        }
        Ok(BoundedOpaquePathBytes(bytes))
    }
}

#[derive(Clone, Debug)]
pub struct RepositoryPath(RepositoryPathRepr);

#[derive(Clone, Debug)]
enum RepositoryPathRepr {
    Utf8(String),
    UnixBytes(Vec<u8>),
}

impl PartialEq for RepositoryPath {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for RepositoryPath {}

impl Hash for RepositoryPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl PartialOrd for RepositoryPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RepositoryPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl RepositoryPath {
    pub fn utf8(value: String) -> Result<Self, RepositoryPathError> {
        validate_repository_path(value.as_bytes())?;
        Ok(Self(RepositoryPathRepr::Utf8(value)))
    }

    pub fn unix_bytes(value: Vec<u8>) -> Result<Self, RepositoryPathError> {
        validate_repository_path(&value)?;
        Ok(Self(RepositoryPathRepr::UnixBytes(value)))
    }

    pub fn byte_len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn display_text(&self) -> Cow<'_, str> {
        match &self.0 {
            RepositoryPathRepr::Utf8(value) => Cow::Borrowed(value),
            RepositoryPathRepr::UnixBytes(value) => String::from_utf8_lossy(value),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            RepositoryPathRepr::Utf8(value) => value.as_bytes(),
            RepositoryPathRepr::UnixBytes(value) => value,
        }
    }

    pub fn as_utf8(&self) -> Option<&str> {
        match &self.0 {
            RepositoryPathRepr::Utf8(value) => Some(value),
            RepositoryPathRepr::UnixBytes(_) => None,
        }
    }

    pub fn as_unix_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            RepositoryPathRepr::Utf8(_) => None,
            RepositoryPathRepr::UnixBytes(value) => Some(value),
        }
    }

    pub fn is_descendant_of(&self, ancestor: &Self) -> bool {
        let path = self.as_bytes();
        let ancestor = ancestor.as_bytes();
        path.len() > ancestor.len()
            && path.starts_with(ancestor)
            && path.get(ancestor.len()) == Some(&b'/')
    }

    pub fn component_count(&self) -> usize {
        self.as_bytes().split(|byte| *byte == b'/').count()
    }

    pub fn depth(&self) -> usize {
        self.component_count() - 1
    }

    pub fn file_name_bytes(&self) -> &[u8] {
        self.as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .expect("validated repository paths have at least one component")
    }

    pub fn file_name_display_text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.file_name_bytes())
    }
}

fn validate_repository_path(value: &[u8]) -> Result<(), RepositoryPathError> {
    if value.is_empty() {
        return Err(RepositoryPathError::Empty);
    }
    if value.len() > MAX_REPOSITORY_PATH_BYTES {
        return Err(RepositoryPathError::TooLong {
            len: value.len(),
            max: MAX_REPOSITORY_PATH_BYTES,
        });
    }
    if value.contains(&0) {
        return Err(RepositoryPathError::ContainsNul);
    }
    if value.starts_with(b"/") {
        return Err(RepositoryPathError::Absolute);
    }
    for component in value.split(|byte| *byte == b'/') {
        match component {
            b"" => return Err(RepositoryPathError::EmptyComponent),
            b"." => return Err(RepositoryPathError::CurrentDirectoryComponent),
            b".." => return Err(RepositoryPathError::ParentDirectoryComponent),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RepositoryPathError {
    #[error("repository path cannot be empty")]
    Empty,
    #[error("repository path length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("repository path cannot contain a NUL byte")]
    ContainsNul,
    #[error("repository path must be relative")]
    Absolute,
    #[error("repository path cannot contain an empty component")]
    EmptyComponent,
    #[error("repository path cannot contain a current-directory component")]
    CurrentDirectoryComponent,
    #[error("repository path cannot contain a parent-directory component")]
    ParentDirectoryComponent,
}

impl Serialize for RepositoryPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            RepositoryPathRepr::Utf8(value) => serializer.serialize_str(value),
            RepositoryPathRepr::UnixBytes(value) => {
                let mut state = serializer.serialize_struct("RepositoryPath", 2)?;
                state.serialize_field("kind", "unix-bytes")?;
                state.serialize_field("bytes", value)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RepositoryPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RepositoryPathVisitor)
    }
}

struct RepositoryPathVisitor;

impl<'de> Visitor<'de> for RepositoryPathVisitor {
    type Value = RepositoryPath;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "a bounded canonical relative UTF-8 repository path string or strict unix-bytes object",
        )
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        RepositoryPath::utf8(value.to_owned()).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        RepositoryPath::utf8(value).map_err(E::custom)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut kind = None;
        let mut bytes = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(serde::de::Error::duplicate_field("kind"));
                    }
                    kind = Some(map.next_value::<String>()?);
                }
                "bytes" => {
                    if bytes.is_some() {
                        return Err(serde::de::Error::duplicate_field("bytes"));
                    }
                    bytes = Some(map.next_value::<BoundedRepositoryPathBytes>()?.0);
                }
                _ => {
                    return Err(serde::de::Error::unknown_field(&field, &["kind", "bytes"]));
                }
            }
        }
        let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
        if kind != "unix-bytes" {
            return Err(serde::de::Error::unknown_variant(&kind, &["unix-bytes"]));
        }
        let bytes = bytes.ok_or_else(|| serde::de::Error::missing_field("bytes"))?;
        RepositoryPath::unix_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

struct BoundedRepositoryPathBytes(Vec<u8>);

impl<'de> Deserialize<'de> for BoundedRepositoryPathBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedRepositoryPathBytesVisitor)
    }
}

struct BoundedRepositoryPathBytesVisitor;

impl<'de> Visitor<'de> for BoundedRepositoryPathBytesVisitor {
    type Value = BoundedRepositoryPathBytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at most {MAX_REPOSITORY_PATH_BYTES} repository path bytes")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(
            sequence.size_hint().unwrap_or(0).min(MAX_REPOSITORY_PATH_BYTES),
        );
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == MAX_REPOSITORY_PATH_BYTES {
                return Err(serde::de::Error::invalid_length(bytes.len() + 1, &self));
            }
            bytes.push(byte);
        }
        Ok(BoundedRepositoryPathBytes(bytes))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PathSemantics {
    pub style: PathStyle,
    pub encoding: PathEncoding,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalTransportKind {
    WindowsNamedPipe,
    UnixDomainSocket,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostDescriptor {
    pub operating_system: OperatingSystemId,
    pub architecture: ArchitectureId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateSchemaSupport {
    pub versions: ProtocolRange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderContractSupport {
    pub provider: AgentId,
    pub revision: ProviderContractRevision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderAdapterContractSupport {
    pub provider: AgentId,
    pub family: AdapterFamily,
    pub adapter_id: AdapterId,
    pub revision: AdapterContractRevision,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderContractManifestError {
    #[error("provider contract count {len} exceeds the {max}-entry limit")]
    TooManyProviders { len: usize, max: usize },
    #[error("provider adapter contract count {len} exceeds the {max}-entry limit")]
    TooManyAdapterContracts { len: usize, max: usize },
    #[error("provider contract manifest contains duplicate provider {provider:?}")]
    DuplicateProvider { provider: AgentId },
    #[error("provider adapter contract for {provider:?} has no provider contract")]
    UnlinkedAdapterProvider { provider: AgentId },
    #[error("provider adapter contract manifest contains duplicate family {family:?} for {provider:?}")]
    DuplicateProviderFamily {
        provider: AgentId,
        family: AdapterFamily,
    },
}

pub fn validate_provider_contract_manifest(
    provider_contracts: &[ProviderContractSupport],
    provider_adapter_contracts: &[ProviderAdapterContractSupport],
) -> Result<(), ProviderContractManifestError> {
    if provider_contracts.len() > MAX_PROVIDER_CONTRACTS {
        return Err(ProviderContractManifestError::TooManyProviders {
            len: provider_contracts.len(),
            max: MAX_PROVIDER_CONTRACTS,
        });
    }
    if provider_adapter_contracts.len() > MAX_PROVIDER_ADAPTER_CONTRACTS {
        return Err(ProviderContractManifestError::TooManyAdapterContracts {
            len: provider_adapter_contracts.len(),
            max: MAX_PROVIDER_ADAPTER_CONTRACTS,
        });
    }
    for (index, contract) in provider_contracts.iter().enumerate() {
        if provider_contracts[..index]
            .iter()
            .any(|existing| existing.provider == contract.provider)
        {
            return Err(ProviderContractManifestError::DuplicateProvider {
                provider: contract.provider.clone(),
            });
        }
    }
    for (index, contract) in provider_adapter_contracts.iter().enumerate() {
        if !provider_contracts
            .iter()
            .any(|provider| provider.provider == contract.provider)
        {
            return Err(ProviderContractManifestError::UnlinkedAdapterProvider {
                provider: contract.provider.clone(),
            });
        }
        if provider_adapter_contracts[..index].iter().any(|existing| {
            existing.provider == contract.provider && existing.family == contract.family
        }) {
            return Err(ProviderContractManifestError::DuplicateProviderFamily {
                provider: contract.provider.clone(),
                family: contract.family,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientCompatibilityOffer {
    pub protocol_versions: ProtocolRange,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<StateSchemaSupport>,
}

impl ClientCompatibilityOffer {
    pub fn exact(protocol_version: u16) -> Result<Self, ProtocolNegotiationError> {
        Ok(Self {
            protocol_versions: ProtocolRange::exact(protocol_version)?,
            capabilities: Vec::new(),
            state_schema: None,
        })
    }
}

pub fn production_node_client_compatibility_offer() -> ClientCompatibilityOffer {
    ClientCompatibilityOffer {
        protocol_versions: ProtocolRange {
            minimum: NODE_PROTOCOL_VERSION,
            maximum: NODE_PROTOCOL_VERSION,
        },
        capabilities: [
            NODE_COMPATIBILITY_METADATA_CAPABILITY,
            NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY,
            NODE_HARNESS_MCP_READ_PROXY_CAPABILITY,
            CAPABILITY_HOST_DIRECTORY_BROWSE_V1,
            NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY,
            NODE_OPAQUE_UNIX_PATH_CAPABILITY,
            NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY,
            NODE_PROVIDER_ID_OPEN_CAPABILITY,
            NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY,
            NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY,
            NODE_REPOSITORY_PATH_CAPABILITY,
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
            NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
            NODE_NATIVE_SESSION_CATALOG_CAPABILITY,
            NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY,
            NODE_NATIVE_SESSION_INDEX_CAPABILITY,
            NODE_NATIVE_SESSION_PREVIEW_CAPABILITY,
            NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
            NODE_SESSION_TASK_CORRELATION_CAPABILITY,
            NODE_OBSERVATION_EVENTS_CAPABILITY,
            NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
            NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
            NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY,
            NODE_SPAWN_PROFILE_REVISION_CAPABILITY,
            NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
            NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
            NODE_WORKSPACE_FILE_READ_CAPABILITY,
            NODE_WORKSPACE_FILE_WRITE_CAPABILITY,
            NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY,
            NODE_GIT_READ_CAPABILITY,
            NODE_WORKTREE_SELECTION_CAPABILITY,
        ]
        .into_iter()
        .map(|capability| CapabilityId(capability.to_owned()))
        .collect(),
        state_schema: Some(StateSchemaSupport {
            versions: ProtocolRange {
                minimum: NODE_STATE_SCHEMA_V1,
                maximum: NODE_STATE_SCHEMA_V10,
            },
        }),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeCompatibilitySupport {
    pub protocol_versions: ProtocolRange,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
    pub local_transport: LocalTransportKind,
    pub state_schema: StateSchemaSupport,
    #[serde(default)]
    pub provider_contracts: Vec<ProviderContractSupport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_adapter_contracts: Vec<ProviderAdapterContractSupport>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiatedNodeCompatibility {
    pub protocol_version: u16,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
    pub local_transport: LocalTransportKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_schema_version: Option<u16>,
    #[serde(default)]
    pub provider_contracts: Vec<ProviderContractSupport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_adapter_contracts: Vec<ProviderAdapterContractSupport>,
}

impl NodeCompatibilitySupport {
    pub fn negotiate(
        &self,
        active_protocol_version: u16,
        client: &ClientCompatibilityOffer,
    ) -> Result<NegotiatedNodeCompatibility, ProtocolNegotiationError> {
        if !self.protocol_versions.contains(active_protocol_version)
            || !client.protocol_versions.contains(active_protocol_version)
        {
            return Err(ProtocolNegotiationError::ActiveVersionUnsupported {
                active: active_protocol_version,
                local: self.protocol_versions,
                remote: client.protocol_versions,
            });
        }
        let capabilities: Vec<CapabilityId> = self
            .capabilities
            .iter()
            .filter(|capability| client.capabilities.contains(capability))
            .cloned()
            .collect();
        let state_schema_version = match client.state_schema {
            Some(client_state) => Some(
                self.state_schema
                    .versions
                    .highest_common(client_state.versions)?,
            ),
            None => None,
        };
        let provider_manifest_selected = capabilities.iter().any(|capability| {
            capability.as_str() == NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY
        });
        if provider_manifest_selected {
            validate_provider_contract_manifest(
                &self.provider_contracts,
                &self.provider_adapter_contracts,
            )
            .map_err(ProtocolNegotiationError::InvalidProviderContractManifest)?;
        }
        Ok(NegotiatedNodeCompatibility {
            protocol_version: active_protocol_version,
            capabilities,
            host: self.host.clone(),
            path_semantics: self.path_semantics.clone(),
            local_transport: self.local_transport,
            state_schema_version,
            provider_contracts: provider_manifest_selected
                .then(|| self.provider_contracts.clone())
                .unwrap_or_default(),
            provider_adapter_contracts: provider_manifest_selected
                .then(|| self.provider_adapter_contracts.clone())
                .unwrap_or_default(),
        })
    }
}

#[derive(Serialize)]
struct NodeCompatibilityAuthBinding<'a> {
    offer: &'a ClientCompatibilityOffer,
    selected: &'a NegotiatedNodeCompatibility,
}

pub fn encode_node_compatibility_auth_binding(
    offer: &ClientCompatibilityOffer,
    selected: &NegotiatedNodeCompatibility,
) -> Result<Vec<u8>, NodeCompatibilityAuthBindingError> {
    let encoded = serde_json::to_vec(&NodeCompatibilityAuthBinding { offer, selected })?;
    if encoded.len() > MAX_NODE_HELLO_FRAME_BYTES {
        return Err(NodeCompatibilityAuthBindingError::TooLarge {
            len: encoded.len(),
            max: MAX_NODE_HELLO_FRAME_BYTES,
        });
    }
    Ok(encoded)
}

#[derive(Debug, Error)]
pub enum NodeCompatibilityAuthBindingError {
    #[error("node compatibility authentication binding serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("node compatibility authentication binding length {len} exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
}

pub fn validate_node_negotiated_handshake_capacity(
    support: &NodeCompatibilitySupport,
    active_protocol_version: u16,
) -> Result<(), NodeNegotiatedHandshakeCapacityError> {
    let offer = production_node_client_compatibility_offer();
    let selected = support.negotiate(active_protocol_version, &offer)?;
    encode_node_compatibility_auth_binding(&offer, &selected)?;
    validate_node_handshake_frame_capacity(
        "negotiated client hello",
        &ClientFrame::Hello(ClientHello {
            protocol_version: active_protocol_version,
            role: ClientRole::Observer,
            client_nonce: [u8::MAX; NODE_AUTH_NONCE_BYTES],
            compatibility: Some(offer),
        }),
    )?;
    validate_node_handshake_frame_capacity(
        "negotiated server challenge",
        &ServerFrame::Challenge(ServerChallenge {
            protocol_version: active_protocol_version,
            server_nonce: [u8::MAX; NODE_AUTH_NONCE_BYTES],
            server_proof: [u8::MAX; NODE_AUTH_PROOF_BYTES],
            compatibility: Some(selected),
        }),
    )
}

fn validate_node_handshake_frame_capacity<T>(
    frame: &'static str,
    value: &T,
) -> Result<(), NodeNegotiatedHandshakeCapacityError>
where
    T: Serialize,
{
    let encoded = serde_json::to_vec(value).map_err(|source| {
        NodeNegotiatedHandshakeCapacityError::Serialization { frame, source }
    })?;
    if encoded.is_empty() || encoded.len() > MAX_NODE_HELLO_FRAME_BYTES {
        return Err(NodeNegotiatedHandshakeCapacityError::FrameTooLarge {
            frame,
            len: encoded.len(),
            max: MAX_NODE_HELLO_FRAME_BYTES,
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum NodeNegotiatedHandshakeCapacityError {
    #[error(transparent)]
    Negotiation(#[from] ProtocolNegotiationError),
    #[error(transparent)]
    AuthenticationBinding(#[from] NodeCompatibilityAuthBindingError),
    #[error("{frame} JSON serialization failed: {source}")]
    Serialization {
        frame: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("{frame} length {len} is outside 1..={max}")]
    FrameTooLarge {
        frame: &'static str,
        len: usize,
        max: usize,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SessionKey {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SessionAddress {
    pub workspace_id: WorkspaceId,
    pub session: SessionKey,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedSessionState {
    IdentityPending,
    Live,
    Dormant,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTaskBindingV1 {
    pub revision: u64,
    pub task_id: Option<TaskId>,
    pub changed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SessionTaskTargetV1 {
    New,
    Existing { task_id: TaskId },
    Clear,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ManagedSessionRecord {
    pub record_id: SessionRecordId,
    pub display_name: String,
    pub provider: AgentId,
    pub mode: SessionMode,
    pub state: ManagedSessionState,
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub provider_session: Option<ProviderSessionIdentity>,
    pub active_session: Option<SessionAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<ResolvedBundleReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<SpawnContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ResolvedContextPackReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_binding: Option<SessionTaskBindingV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_error: Option<String>,
}

impl ManagedSessionRecord {
    pub fn context_binding_is_valid(&self) -> bool {
        context_receipt_binding_is_valid(self.context_id.as_ref(), self.context.as_ref())
    }

    pub fn task_binding_is_valid(&self) -> bool {
        self.task_binding
            .as_ref()
            .map_or(true, |binding| {
                binding.revision > 0
                    && binding.changed_at_unix_ms > 0
                    && binding.changed_at_unix_ms >= self.created_at_unix_ms
                    && binding.changed_at_unix_ms <= self.updated_at_unix_ms
            })
    }
}

impl<'de> Deserialize<'de> for ManagedSessionRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRecord {
            record_id: SessionRecordId,
            display_name: String,
            provider: AgentId,
            mode: SessionMode,
            state: ManagedSessionState,
            workspace_id: WorkspaceId,
            canonical_root: OpaqueHostPath,
            provider_session: Option<ProviderSessionIdentity>,
            active_session: Option<SessionAddress>,
            #[serde(default)]
            environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
            #[serde(default)]
            bundle: Option<ResolvedBundleReceipt>,
            #[serde(default)]
            context_id: Option<SpawnContextId>,
            #[serde(default)]
            context: Option<ResolvedContextPackReceipt>,
            #[serde(default)]
            task_binding: Option<SessionTaskBindingV1>,
            created_at_unix_ms: u64,
            updated_at_unix_ms: u64,
            last_error: Option<String>,
        }

        let wire = WireRecord::deserialize(deserializer)?;
        let record = Self {
            record_id: wire.record_id,
            display_name: wire.display_name,
            provider: wire.provider,
            mode: wire.mode,
            state: wire.state,
            workspace_id: wire.workspace_id,
            canonical_root: wire.canonical_root,
            provider_session: wire.provider_session,
            active_session: wire.active_session,
            environment_profile: wire.environment_profile,
            bundle: wire.bundle,
            context_id: wire.context_id,
            context: wire.context,
            task_binding: wire.task_binding,
            created_at_unix_ms: wire.created_at_unix_ms,
            updated_at_unix_ms: wire.updated_at_unix_ms,
            last_error: wire.last_error,
        };
        if !record.context_binding_is_valid() {
            return Err(serde::de::Error::custom(
                "managed session context id and materialization receipt are not correlated",
            ));
        }
        if !record.task_binding_is_valid() {
            return Err(serde::de::Error::custom(
                "managed session task binding revision or timestamp is invalid",
            ));
        }
        Ok(record)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub sessions: Vec<SessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_service_mode: Option<WorktreeServiceMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_worktree_profiles: Option<WorktreeProfileInventory>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeServiceMode {
    Manual,
    Managed,
    Off,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEntry {
    pub relative_path: RepositoryPath,
    pub kind: WorkspaceEntryKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitStatusEntry {
    pub index_status: String,
    pub worktree_status: String,
    pub path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<RepositoryPath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitCommitSummary {
    pub id: String,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct GitObjectId(String);

impl GitObjectId {
    pub fn new(value: String) -> Result<Self, GitObjectIdError> {
        if !matches!(value.len(), 40 | 64)
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(GitObjectIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for GitObjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("git object id must be a 40- or 64-character lowercase hexadecimal digest")]
pub struct GitObjectIdError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitSignatureStatus {
    Good,
    Bad,
    UnknownValidity,
    ExpiredSignature,
    ExpiredKey,
    RevokedKey,
    CannotCheck,
    NoSignature,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitCommitDetails {
    pub id: GitObjectId,
    pub parents: Vec<GitObjectId>,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    pub signature_status: GitSignatureStatus,
    pub signer: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitHistoryPage {
    pub commits: Vec<GitCommitDetails>,
    pub next_before: Option<GitObjectId>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GitDiffMode {
    Working,
    Staged,
    Commit { revision: GitObjectId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitDiffRequest {
    pub mode: GitDiffMode,
    pub path: Option<RepositoryPath>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitDiff {
    pub mode: GitDiffMode,
    pub path: Option<RepositoryPath>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitWorktreeSnapshot {
    pub path: OpaqueHostPath,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_main: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktreeGitScope {
    pub lease_id: ManagedWorktreeLeaseId,
    pub source_workspace_id: WorkspaceId,
    #[serde(deserialize_with = "deserialize_managed_worktree_git_branch")]
    pub branch: String,
    pub base_commit: GitObjectId,
    pub active_session_count: u16,
    pub managed_record_count: u16,
}

fn deserialize_managed_worktree_git_branch<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let branch = String::deserialize(deserializer)?;
    if branch.is_empty() || branch.len() > MAX_REPOSITORY_PATH_BYTES {
        return Err(serde::de::Error::custom(
            "managed worktree git branch must be non-empty and bounded",
        ));
    }
    Ok(branch)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitSnapshot {
    pub is_repository: bool,
    pub branch: Option<String>,
    pub status: Vec<GitStatusEntry>,
    pub recent_commits: Vec<GitCommitSummary>,
    #[serde(default)]
    pub worktrees: Vec<GitWorktreeSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_worktree: Option<ManagedWorktreeGitScope>,
    pub truncated: bool,
    pub diagnostic: Option<String>,
}

impl GitSnapshot {
    pub fn managed_worktree_is_valid_for(&self, workspace_id: &WorkspaceId) -> bool {
        self.managed_worktree.as_ref().map_or(true, |scope| {
            self.is_repository
                && self.branch.as_deref() == Some(scope.branch.as_str())
                && &scope.source_workspace_id != workspace_id
                && (u32::from(scope.active_session_count)
                    + u32::from(scope.managed_record_count))
                    > 0
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceInspection {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<WorkspaceEntry>,
    pub tree_truncated: bool,
    pub git: GitSnapshot,
}

impl<'de> Deserialize<'de> for WorkspaceInspection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireInspection {
            workspace_id: WorkspaceId,
            entries: Vec<WorkspaceEntry>,
            tree_truncated: bool,
            git: GitSnapshot,
        }

        let wire = WireInspection::deserialize(deserializer)?;
        if !wire.git.managed_worktree_is_valid_for(&wire.workspace_id) {
            return Err(serde::de::Error::custom(
                "managed worktree git scope is inconsistent with workspace inspection",
            ));
        }
        Ok(Self {
            workspace_id: wire.workspace_id,
            entries: wire.entries,
            tree_truncated: wire.tree_truncated,
            git: wire.git,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostDirectoryEntry {
    pub path: OpaqueHostPath,
    pub display_name: String,
    pub is_link: bool,
}

impl HostDirectoryEntry {
    pub fn new(
        path: OpaqueHostPath,
        display_name: String,
        is_link: bool,
    ) -> Result<Self, HostDirectoryEntryError> {
        if path.as_utf8().is_none() {
            return Err(HostDirectoryEntryError::NonUtf8Path);
        }
        if display_name.is_empty() {
            return Err(HostDirectoryEntryError::EmptyDisplayName);
        }
        if display_name.len() > MAX_HOST_DIRECTORY_DISPLAY_NAME_BYTES {
            return Err(HostDirectoryEntryError::DisplayNameTooLong {
                len: display_name.len(),
                max: MAX_HOST_DIRECTORY_DISPLAY_NAME_BYTES,
            });
        }
        if display_name.chars().any(char::is_control) {
            return Err(HostDirectoryEntryError::ControlCharacter);
        }
        Ok(Self { path, display_name, is_link })
    }
}

impl<'de> Deserialize<'de> for HostDirectoryEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEntry {
            path: OpaqueHostPath,
            display_name: String,
            is_link: bool,
        }

        let wire = WireEntry::deserialize(deserializer)?;
        Self::new(wire.path, wire.display_name, wire.is_link)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HostDirectoryEntryError {
    #[error("host directory path must use the UTF-8 wire representation")]
    NonUtf8Path,
    #[error("host directory display name cannot be empty")]
    EmptyDisplayName,
    #[error("host directory display name length {len} exceeds the {max}-byte limit")]
    DisplayNameTooLong { len: usize, max: usize },
    #[error("host directory display name cannot contain control characters")]
    ControlCharacter,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostDirectoryListing {
    pub directory: Option<OpaqueHostPath>,
    pub parent: Option<OpaqueHostPath>,
    #[serde(deserialize_with = "deserialize_host_directory_entries")]
    pub entries: Vec<HostDirectoryEntry>,
    pub next_after: Option<OpaqueHostPath>,
    /// True only when another page of supported directory entries is available.
    pub incomplete: bool,
}

fn deserialize_host_directory_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<HostDirectoryEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    struct HostDirectoryEntriesVisitor;

    impl<'de> Visitor<'de> for HostDirectoryEntriesVisitor {
        type Value = Vec<HostDirectoryEntry>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_HOST_DIRECTORY_ENTRIES} host directory entries")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut entries = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_HOST_DIRECTORY_ENTRIES),
            );
            while let Some(entry) = sequence.next_element::<HostDirectoryEntry>()? {
                if entries.len() == MAX_HOST_DIRECTORY_ENTRIES {
                    return Err(serde::de::Error::invalid_length(entries.len() + 1, &self));
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_seq(HostDirectoryEntriesVisitor)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceFileRead {
    pub workspace_id: WorkspaceId,
    pub path: RepositoryPath,
    pub content: WorkspaceFileContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<WorkspaceFileRevision>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorkspaceFileRevision(String);

impl WorkspaceFileRevision {
    pub fn new(value: String) -> Result<Self, WorkspaceFileRevisionError> {
        if value.len() != 64
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(WorkspaceFileRevisionError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WorkspaceFileRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("workspace file revision must be a 64-character lowercase SHA-256 digest")]
pub struct WorkspaceFileRevisionError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkspaceFileContent {
    Utf8 { text: String, byte_len: u32 },
    NonUtf8 { byte_len: u32 },
    TooLarge { limit_bytes: u32 },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum WorkspaceFileContentWire {
    Utf8 { text: String, byte_len: u32 },
    NonUtf8 { byte_len: u32 },
    TooLarge { limit_bytes: u32 },
}

impl<'de> Deserialize<'de> for WorkspaceFileContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceFileContentWire::deserialize(deserializer)?;
        let limit_bytes = MAX_WORKSPACE_FILE_BYTES as u32;
        match wire {
            WorkspaceFileContentWire::Utf8 { text, byte_len } => {
                let actual_bytes = text.len();
                if actual_bytes > MAX_WORKSPACE_FILE_BYTES {
                    return Err(serde::de::Error::custom(format!(
                        "workspace file content length {actual_bytes} exceeds the {MAX_WORKSPACE_FILE_BYTES}-byte limit",
                    )));
                }
                if u64::from(byte_len) != actual_bytes as u64 {
                    return Err(serde::de::Error::custom(format!(
                        "workspace file content declares {byte_len} bytes but contains {actual_bytes}",
                    )));
                }
                Ok(Self::Utf8 { text, byte_len })
            }
            WorkspaceFileContentWire::NonUtf8 { byte_len } => {
                if byte_len > limit_bytes {
                    return Err(serde::de::Error::custom(format!(
                        "workspace non-UTF-8 file length {byte_len} exceeds the {MAX_WORKSPACE_FILE_BYTES}-byte limit",
                    )));
                }
                Ok(Self::NonUtf8 { byte_len })
            }
            WorkspaceFileContentWire::TooLarge {
                limit_bytes: declared_limit,
            } => {
                if declared_limit != limit_bytes {
                    return Err(serde::de::Error::custom(format!(
                        "workspace file limit {declared_limit} does not match protocol limit {limit_bytes}",
                    )));
                }
                Ok(Self::TooLarge { limit_bytes })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProgressCurrentV1 {
    Idle,
    Working,
    WaitingForInput,
    Blocked,
}

impl From<ProviderActivity> for AgentProgressCurrentV1 {
    fn from(activity: ProviderActivity) -> Self {
        match activity {
            ProviderActivity::Idle => Self::Idle,
            ProviderActivity::Working => Self::Working,
            ProviderActivity::WaitingForInput => Self::WaitingForInput,
            ProviderActivity::Blocked => Self::Blocked,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProgressAttentionKindV1 {
    Approval,
    Question,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProgressAttentionV1 {
    pub kind: AgentProgressAttentionKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_label: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProgressEventKindV1 {
    SessionStarted,
    SessionIdentityObserved,
    TurnStarted,
    WorkingObserved,
    Text,
    Thinking,
    ToolStarted,
    ToolCompleted,
    TurnCompleted,
    TurnInterrupted,
    SessionEnded,
    Error,
    Ready,
    InteractionRequested,
    InteractionResolved,
    SubagentStarted,
    SubagentStopped,
    RateLimited,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProgressUsageV1 {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProgressV1 {
    pub provider_sequence: u64,
    pub activity: ProviderActivity,
    pub completed_turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentProgressUsageV1>,
    pub current: AgentProgressCurrentV1,
    pub active_tool_labels: Vec<String>,
    pub active_tool_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AgentProgressAttentionV1>,
    pub subagent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_kind: Option<AgentProgressEventKindV1>,
    pub gap_count: u64,
    pub stale: bool,
    pub truncated: bool,
}

impl<'de> Deserialize<'de> for AgentProgressV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireProgress {
            provider_sequence: u64,
            activity: ProviderActivity,
            completed_turns: u64,
            #[serde(default)]
            usage: Option<AgentProgressUsageV1>,
            current: AgentProgressCurrentV1,
            active_tool_labels: Vec<String>,
            active_tool_count: u32,
            #[serde(default)]
            attention: Option<AgentProgressAttentionV1>,
            subagent_count: u32,
            #[serde(default)]
            last_event_kind: Option<AgentProgressEventKindV1>,
            gap_count: u64,
            stale: bool,
            truncated: bool,
        }

        let wire = WireProgress::deserialize(deserializer)?;
        if wire.active_tool_labels.len() > MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS {
            return Err(serde::de::Error::custom(
                "agent progress contains too many active tool labels",
            ));
        }
        if usize::try_from(wire.active_tool_count).unwrap_or(usize::MAX)
            < wire.active_tool_labels.len()
        {
            return Err(serde::de::Error::custom(
                "agent progress active tool count is smaller than its labels",
            ));
        }
        for label in &wire.active_tool_labels {
            validate_agent_progress_tool_label(label)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(label) = wire
            .attention
            .as_ref()
            .and_then(|attention| attention.tool_label.as_deref())
        {
            validate_agent_progress_tool_label(label)
                .map_err(serde::de::Error::custom)?;
        }
        if wire.current != AgentProgressCurrentV1::from(wire.activity) {
            return Err(serde::de::Error::custom(
                "agent progress current state conflicts with provider activity",
            ));
        }
        Ok(Self {
            provider_sequence: wire.provider_sequence,
            activity: wire.activity,
            completed_turns: wire.completed_turns,
            usage: wire.usage,
            current: wire.current,
            active_tool_labels: wire.active_tool_labels,
            active_tool_count: wire.active_tool_count,
            attention: wire.attention,
            subagent_count: wire.subagent_count,
            last_event_kind: wire.last_event_kind,
            gap_count: wire.gap_count,
            stale: wire.stale,
            truncated: wire.truncated,
        })
    }
}

fn validate_agent_progress_tool_label(label: &str) -> Result<(), &'static str> {
    if !matches!(
        label,
        "Read" | "Write" | "Edit" | "Shell" | "Search" | "Browse" | "Git"
            | "Ask" | "Task" | "Tool"
    ) {
        return Err("agent progress tool label is outside the safe class vocabulary");
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAgentProgress {
    pub address: SessionAddress,
    pub progress: AgentProgressV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeSnapshot {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentId>,
    #[serde(default, skip_serializing_if = "ProviderRuntimeStatuses::is_empty")]
    pub provider_runtime_statuses: ProviderRuntimeStatuses,
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default)]
    pub session_records: Vec<ManagedSessionRecord>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_managed_worktree_leases"
    )]
    pub managed_worktrees: Vec<ManagedWorktreeLeaseSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_inventory: Option<LaunchInventory>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_agent_progress_entries"
    )]
    pub agent_progress: Vec<SessionAgentProgress>,
}

impl NodeSnapshot {
    pub fn requires_child_environment_profile_capability(&self) -> bool {
        self.session_records
            .iter()
            .any(|record| record.environment_profile.is_some())
    }

    pub fn requires_session_bundle_materialization_capability(&self) -> bool {
        self.session_records
            .iter()
            .any(|record| record.bundle.is_some())
    }

    pub fn requires_history_context_pack_capability(&self) -> bool {
        self.session_records.iter().any(|record| {
            record.context_id.is_some() || record.context.is_some()
        })
    }
}

fn deserialize_agent_progress_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<SessionAgentProgress>, D::Error>
where
    D: Deserializer<'de>,
{
    struct AgentProgressEntriesVisitor;

    impl<'de> Visitor<'de> for AgentProgressEntriesVisitor {
        type Value = Vec<SessionAgentProgress>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_AGENT_PROGRESS_ENTRIES} bounded agent progress entries",
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut entries = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_AGENT_PROGRESS_ENTRIES),
            );
            while let Some(value) = sequence.next_element::<serde_json::Value>()? {
                if entries.len() == MAX_AGENT_PROGRESS_ENTRIES {
                    continue;
                }
                let Ok(encoded) = serde_json::to_vec(&value) else {
                    continue;
                };
                if encoded.len() > MAX_AGENT_PROGRESS_ENTRY_BYTES {
                    continue;
                }
                let Ok(entry) = serde_json::from_value::<SessionAgentProgress>(value) else {
                    continue;
                };
                if entries.iter().any(|existing: &SessionAgentProgress| {
                    existing.address == entry.address
                }) {
                    continue;
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_seq(AgentProgressEntriesVisitor)
}

fn deserialize_history_discovery_limit<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let limit = u16::deserialize(deserializer)?;
    if !(1..=HISTORY_DISCOVERY_LIMIT_MAX).contains(&limit) {
        return Err(serde::de::Error::custom(
            "history discovery limit is outside the supported bounded range",
        ));
    }
    Ok(limit)
}

fn deserialize_native_session_catalog_limit<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let limit = u16::deserialize(deserializer)?;
    if !(1..=NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit) {
        return Err(serde::de::Error::custom(
            "native session catalog limit is outside the supported bounded range",
        ));
    }
    Ok(limit)
}

fn deserialize_native_session_catalog_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<NativeSessionCatalogEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries = Vec::<NativeSessionCatalogEntry>::deserialize(deserializer)?;
    if entries.len() > usize::from(NATIVE_SESSION_CATALOG_LIMIT_MAX) {
        return Err(serde::de::Error::custom(
            "native session catalog exceeds the supported bounded range",
        ));
    }
    for (index, entry) in entries.iter().enumerate() {
        entry.validate().map_err(serde::de::Error::custom)?;
        if entries[..index]
            .iter()
            .any(|existing| existing.selection_id == entry.selection_id)
        {
            return Err(serde::de::Error::custom(
                "native session catalog contains a duplicate selection ID",
            ));
        }
    }
    Ok(entries)
}

fn validate_native_session_catalog_entries(
    route: &NativeSessionCatalogRoute,
    entries: &[NativeSessionCatalogEntry],
) -> Result<(), &'static str> {
    if entries.len() > usize::from(NATIVE_SESSION_CATALOG_LIMIT_MAX) {
        return Err("native session catalog exceeds the bounded entry limit");
    }
    for (index, entry) in entries.iter().enumerate() {
        entry.validate_for_route(route)?;
        if entries[..index]
            .iter()
            .any(|existing| existing.selection_id == entry.selection_id)
        {
            return Err("native session catalog contains duplicate selections");
        }
        if entry.record_id.as_ref().is_some_and(|record_id| {
            entries[..index]
                .iter()
                .any(|existing| existing.record_id.as_ref() == Some(record_id))
        }) {
            return Err("native session catalog contains duplicate managed records");
        }
    }
    Ok(())
}

fn deserialize_optional_native_session_catalog_summary<'de, D>(
    deserializer: D,
) -> Result<Option<NativeSessionCatalogSummary>, D::Error>
where
    D: Deserializer<'de>,
{
    let summary = Option::<NativeSessionCatalogSummary>::deserialize(deserializer)?;
    if let Some(summary) = summary.as_ref() {
        summary.validate().map_err(serde::de::Error::custom)?;
    }
    Ok(summary)
}

fn deserialize_native_session_catalog_page<'de, D>(
    deserializer: D,
) -> Result<NativeSessionCatalogPage, D::Error>
where
    D: Deserializer<'de>,
{
    let page = NativeSessionCatalogPage::deserialize(deserializer)?;
    page.validate().map_err(serde::de::Error::custom)?;
    Ok(page)
}

fn deserialize_native_session_preview_limit<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let limit = u16::deserialize(deserializer)?;
    if !(1..=NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX).contains(&limit) {
        return Err(serde::de::Error::custom(
            "native session preview limit is outside the supported bounded range",
        ));
    }
    Ok(limit)
}

fn deserialize_native_session_preview<'de, D>(
    deserializer: D,
) -> Result<NativeSessionPreview, D::Error>
where
    D: Deserializer<'de>,
{
    let preview = NativeSessionPreview::deserialize(deserializer)?;
    preview.validate().map_err(serde::de::Error::custom)?;
    Ok(preview)
}

fn deserialize_session_record_preview<'de, D>(
    deserializer: D,
) -> Result<SessionRecordPreview, D::Error>
where
    D: Deserializer<'de>,
{
    let preview = SessionRecordPreview::deserialize(deserializer)?;
    preview.validate().map_err(serde::de::Error::custom)?;
    Ok(preview)
}

fn deserialize_history_candidate_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let candidate_id = String::deserialize(deserializer)?;
    gate4agent_types::validate_candidate_id(&candidate_id)
        .map_err(serde::de::Error::custom)?;
    Ok(candidate_id)
}

fn deserialize_optional_history_candidate_id<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let candidate_id = Option::<String>::deserialize(deserializer)?;
    if let Some(candidate_id) = candidate_id.as_deref() {
        gate4agent_types::validate_candidate_id(candidate_id)
            .map_err(serde::de::Error::custom)?;
    }
    Ok(candidate_id)
}

fn deserialize_history_session_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let session_id = String::deserialize(deserializer)?;
    let validation = gate4agent_types::HistorySessionRecord {
        session_id: session_id.clone(),
        title: None,
        cwd: None,
        model: None,
        message_count: 0,
        completed_turn_count: None,
        total_tokens: 0,
        messages: Vec::new(),
    };
    validation.validate().map_err(serde::de::Error::custom)?;
    Ok(session_id)
}

fn deserialize_history_candidates<'de, D>(
    deserializer: D,
) -> Result<Vec<HistoryCandidateSummary>, D::Error>
where
    D: Deserializer<'de>,
{
    let candidates = Vec::<HistoryCandidateSummary>::deserialize(deserializer)?;
    if candidates.len() > usize::from(HISTORY_DISCOVERY_LIMIT_MAX) {
        return Err(serde::de::Error::custom(
            "history candidate count exceeds the discovery limit",
        ));
    }
    for (index, candidate) in candidates.iter().enumerate() {
        candidate.validate().map_err(serde::de::Error::custom)?;
        if candidates[..index]
            .iter()
            .any(|existing| existing.id == candidate.id)
        {
            return Err(serde::de::Error::custom(
                "history candidates contain a duplicate candidate ID",
            ));
        }
    }
    Ok(candidates)
}

fn deserialize_managed_worktree_leases<'de, D>(
    deserializer: D,
) -> Result<Vec<ManagedWorktreeLeaseSnapshot>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ManagedWorktreeLeasesVisitor;

    impl<'de> Visitor<'de> for ManagedWorktreeLeasesVisitor {
        type Value = Vec<ManagedWorktreeLeaseSnapshot>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_MANAGED_WORKTREE_LEASES} managed worktree leases",
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut leases = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_MANAGED_WORKTREE_LEASES),
            );
            while let Some(lease) = sequence.next_element::<ManagedWorktreeLeaseSnapshot>()? {
                if leases.len() == MAX_MANAGED_WORKTREE_LEASES {
                    return Err(serde::de::Error::invalid_length(leases.len() + 1, &self));
                }
                if leases.iter().any(|existing: &ManagedWorktreeLeaseSnapshot| {
                    existing.lease_id == lease.lease_id
                }) {
                    return Err(serde::de::Error::custom(
                        "managed worktree snapshot contains a duplicate lease ID",
                    ));
                }
                if leases.iter().any(|existing: &ManagedWorktreeLeaseSnapshot| {
                    existing.workspace_id == lease.workspace_id
                }) {
                    return Err(serde::de::Error::custom(
                        "managed worktree snapshot contains a duplicate workspace ID",
                    ));
                }
                leases.push(lease);
            }
            Ok(leases)
        }
    }

    deserializer.deserialize_seq(ManagedWorktreeLeasesVisitor)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    pub protocol_version: u16,
    pub role: ClientRole,
    pub client_nonce: [u8; NODE_AUTH_NONCE_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<ClientCompatibilityOffer>,
}

impl ClientHello {
    pub fn new(role: ClientRole, client_nonce: [u8; NODE_AUTH_NONCE_BYTES]) -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            role,
            client_nonce,
            compatibility: None,
        }
    }

    pub fn negotiating(
        role: ClientRole,
        client_nonce: [u8; NODE_AUTH_NONCE_BYTES],
        compatibility: ClientCompatibilityOffer,
    ) -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            role,
            client_nonce,
            compatibility: Some(compatibility),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerChallenge {
    pub protocol_version: u16,
    pub server_nonce: [u8; NODE_AUTH_NONCE_BYTES],
    pub server_proof: [u8; NODE_AUTH_PROOF_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedNodeCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientAuthentication {
    pub client_proof: [u8; NODE_AUTH_PROOF_BYTES],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControllerState {
    pub connection_id: u64,
    pub lease_remaining_ms: u64,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DeliveryBlobDigestV1(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DeliveryManifestDigestV2(String);

macro_rules! delivery_digest_impl {
    ($type:ident, $label:literal) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, DeliveryDigestError> {
                let value = value.into();
                let hex = value.strip_prefix("sha256:").ok_or(DeliveryDigestError($label))?;
                if hex.len() != 64
                    || !hex.bytes().all(|byte| {
                        byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
                    })
                {
                    return Err(DeliveryDigestError($label));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = DeliveryDigestError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

delivery_digest_impl!(DeliveryBlobDigestV1, "delivery blob");
delivery_digest_impl!(DeliveryManifestDigestV2, "delivery manifest");

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("{0} digest must be sha256: followed by exactly 64 lowercase hexadecimal characters")]
pub struct DeliveryDigestError(&'static str);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DeliveryStageId(String);

impl DeliveryStageId {
    pub fn new(value: impl Into<String>) -> Result<Self, DeliveryStageIdError> {
        let value = value.into();
        let hex = value
            .strip_prefix("delivery-stage-")
            .ok_or(DeliveryStageIdError)?;
        if hex.len() != DELIVERY_STAGE_NONCE_BYTES * 2
            || !hex.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            })
        {
            return Err(DeliveryStageIdError);
        }
        Ok(Self(value))
    }

    pub fn from_nonce(nonce: [u8; DELIVERY_STAGE_NONCE_BYTES]) -> Self {
        let mut value = String::with_capacity(15 + DELIVERY_STAGE_NONCE_BYTES * 2);
        value.push_str("delivery-stage-");
        for byte in nonce {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeliveryStageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DeliveryStageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("delivery stage ID must be delivery-stage- followed by exactly 32 lowercase hexadecimal characters")]
pub struct DeliveryStageIdError;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DeliveryRelativePathV2(String);

impl DeliveryRelativePathV2 {
    pub fn new(value: impl Into<String>) -> Result<Self, DeliveryRelativePathError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_DELIVERY_RELATIVE_PATH_BYTES
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.chars().any(char::is_control)
            || value
                .split('/')
                .any(|part| !delivery_path_component_is_portable(part))
        {
            return Err(DeliveryRelativePathError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn delivery_path_component_is_portable(component: &str) -> bool {
    let invalid_character = component.chars().any(|character| {
        character <= '\u{1f}'
            || matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    });
    let stem = component.split('.').next().unwrap_or(component);
    let uppercase_stem = stem.to_ascii_uppercase();
    let reserved = matches!(uppercase_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || delivery_reserved_numbered_name(&uppercase_stem, "COM")
        || delivery_reserved_numbered_name(&uppercase_stem, "LPT");
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.ends_with('.')
        && !component.ends_with(' ')
        && !invalid_character
        && !reserved
}

fn delivery_reserved_numbered_name(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
    })
}

impl fmt::Display for DeliveryRelativePathV2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DeliveryRelativePathV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("delivery path must be a safe forward-slash relative path of at most 512 UTF-8 bytes")]
pub struct DeliveryRelativePathError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryScopeV2 {
    Workspace,
    Session,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryComponentKindV2 {
    Skill,
    PluginManifest,
    Prompt,
    Instructions,
    AgentDefinition,
    Command,
    File,
    Template,
    McpDeclaration,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBlobReceiptV1 {
    pub digest: DeliveryBlobDigestV1,
    pub byte_len: u64,
}

impl DeliveryBlobReceiptV1 {
    pub fn new(
        digest: DeliveryBlobDigestV1,
        byte_len: u64,
    ) -> Result<Self, DeliveryContractError> {
        if byte_len > MAX_DELIVERY_FILE_BYTES as u64 {
            return Err(DeliveryContractError::FileTooLarge);
        }
        Ok(Self { digest, byte_len })
    }
}

impl<'de> Deserialize<'de> for DeliveryBlobReceiptV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            digest: DeliveryBlobDigestV1,
            byte_len: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.digest, wire.byte_len).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryComponentV2 {
    pub kind: DeliveryComponentKindV2,
    pub scope: DeliveryScopeV2,
    pub relative_path: DeliveryRelativePathV2,
    pub blob: DeliveryBlobReceiptV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBundleManifestV2 {
    pub bundle_id: SpawnBundleId,
    pub revision: SpawnBundleRevision,
    pub bundle_digest: SpawnBundleDigest,
    pub manifest_digest: DeliveryManifestDigestV2,
    pub components: Vec<DeliveryComponentV2>,
}

impl DeliveryBundleManifestV2 {
    pub fn validate(&self) -> Result<(), DeliveryContractError> {
        if self.components.is_empty() {
            return Err(DeliveryContractError::EmptyManifest);
        }
        if self.components.len() > MAX_DELIVERY_FILES {
            return Err(DeliveryContractError::TooManyFiles);
        }
        let mut total = 0_u64;
        let mut previous_path: Option<&str> = None;
        let mut folded_paths = std::collections::BTreeSet::new();
        for component in &self.components {
            let path = component.relative_path.as_str();
            if previous_path.is_some_and(|previous| previous >= path) {
                return Err(DeliveryContractError::ComponentsNotOrdered);
            }
            previous_path = Some(path);
            let folded = path.to_lowercase();
            if !folded_paths.insert(folded) {
                return Err(DeliveryContractError::CaseFoldPathCollision);
            }
            total = total
                .checked_add(component.blob.byte_len)
                .ok_or(DeliveryContractError::TotalTooLarge)?;
            if total > MAX_DELIVERY_TOTAL_BYTES as u64 {
                return Err(DeliveryContractError::TotalTooLarge);
            }
        }
        Ok(())
    }

    pub fn canonical_manifest_digest_material(&self) -> Vec<u8> {
        fn push_field(material: &mut Vec<u8>, value: &[u8]) {
            material.extend_from_slice(&(value.len() as u32).to_be_bytes());
            material.extend_from_slice(value);
        }

        let mut material = Vec::new();
        material.extend_from_slice(b"g4a-delivery-manifest-v2\0");
        push_field(&mut material, self.bundle_id.as_str().as_bytes());
        push_field(&mut material, self.revision.as_str().as_bytes());
        push_field(&mut material, self.bundle_digest.as_str().as_bytes());
        material.extend_from_slice(&(self.components.len() as u32).to_be_bytes());
        for component in &self.components {
            material.push(component.kind.canonical_tag());
            material.push(component.scope.canonical_tag());
            push_field(&mut material, component.relative_path.as_str().as_bytes());
            push_field(&mut material, component.blob.digest.as_str().as_bytes());
            material.extend_from_slice(&component.blob.byte_len.to_be_bytes());
        }
        material
    }
}

impl DeliveryComponentKindV2 {
    pub fn canonical_tag(self) -> u8 {
        match self {
            Self::Skill => 1,
            Self::PluginManifest => 2,
            Self::Prompt => 3,
            Self::Instructions => 4,
            Self::AgentDefinition => 5,
            Self::Command => 6,
            Self::File => 7,
            Self::Template => 8,
            Self::McpDeclaration => 9,
        }
    }
}

impl DeliveryScopeV2 {
    pub fn canonical_tag(self) -> u8 {
        match self {
            Self::Workspace => 1,
            Self::Session => 2,
        }
    }
}

impl<'de> Deserialize<'de> for DeliveryBundleManifestV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            bundle_id: SpawnBundleId,
            revision: SpawnBundleRevision,
            bundle_digest: SpawnBundleDigest,
            manifest_digest: DeliveryManifestDigestV2,
            components: Vec<DeliveryComponentV2>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let manifest = Self {
            bundle_id: wire.bundle_id,
            revision: wire.revision,
            bundle_digest: wire.bundle_digest,
            manifest_digest: wire.manifest_digest,
            components: wire.components,
        };
        manifest.validate().map_err(serde::de::Error::custom)?;
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DeliveryBlobChunkHexV1(String);

impl DeliveryBlobChunkHexV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, DeliveryContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_DELIVERY_CHUNK_RAW_BYTES * 2
            || value.len() % 2 != 0
            || !value.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            })
        {
            return Err(DeliveryContractError::InvalidChunkHex);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn raw_len(&self) -> usize {
        self.0.len() / 2
    }

    pub fn decode(&self) -> Vec<u8> {
        fn nibble(value: u8) -> u8 {
            match value {
                b'0'..=b'9' => value - b'0',
                b'a'..=b'f' => value - b'a' + 10,
                _ => unreachable!("validated delivery chunk contains only lowercase hex"),
            }
        }

        self.0
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
            .collect()
    }
}

impl<'de> Deserialize<'de> for DeliveryBlobChunkHexV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DeliveryContractError {
    #[error("delivery manifest must contain at least one component")]
    EmptyManifest,
    #[error("delivery manifest exceeds the file count limit")]
    TooManyFiles,
    #[error("delivery file exceeds the byte limit")]
    FileTooLarge,
    #[error("delivery manifest exceeds the total byte limit")]
    TotalTooLarge,
    #[error("delivery components must be strictly ordered by relative path")]
    ComponentsNotOrdered,
    #[error("delivery component paths collide under case folding")]
    CaseFoldPathCollision,
    #[error("delivery blob receipts must be strictly ordered and unique by digest")]
    BlobReceiptsNotOrdered,
    #[error("delivery chunk must encode 1 through 49152 raw bytes as lowercase hexadecimal")]
    InvalidChunkHex,
    #[error("delivery chunk exceeds the declared file bounds")]
    ChunkOutOfBounds,
}

fn validate_delivery_blob_receipts(
    blobs: &[DeliveryBlobReceiptV1],
) -> Result<(), DeliveryContractError> {
    if blobs.len() > MAX_DELIVERY_FILES {
        return Err(DeliveryContractError::TooManyFiles);
    }
    let mut previous: Option<&DeliveryBlobDigestV1> = None;
    for blob in blobs {
        if previous.is_some_and(|digest| digest >= &blob.digest) {
            return Err(DeliveryContractError::BlobReceiptsNotOrdered);
        }
        previous = Some(&blob.digest);
    }
    Ok(())
}

fn deserialize_delivery_blob_receipts<'de, D>(
    deserializer: D,
) -> Result<Vec<DeliveryBlobReceiptV1>, D::Error>
where
    D: Deserializer<'de>,
{
    let blobs = Vec::<DeliveryBlobReceiptV1>::deserialize(deserializer)?;
    validate_delivery_blob_receipts(&blobs).map_err(serde::de::Error::custom)?;
    Ok(blobs)
}

fn deserialize_delivery_blob_digests<'de, D>(
    deserializer: D,
) -> Result<Vec<DeliveryBlobDigestV1>, D::Error>
where
    D: Deserializer<'de>,
{
    let digests = Vec::<DeliveryBlobDigestV1>::deserialize(deserializer)?;
    if digests.len() > MAX_DELIVERY_FILES
        || digests.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(serde::de::Error::custom(
            "delivery blob digests must be bounded, strictly ordered, and unique",
        ));
    }
    Ok(digests)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryCommitReceiptV1 {
    pub bundle_id: SpawnBundleId,
    pub revision: SpawnBundleRevision,
    pub bundle_digest: SpawnBundleDigest,
    pub manifest_digest: DeliveryManifestDigestV2,
    #[serde(deserialize_with = "deserialize_delivery_blob_receipts")]
    pub blobs: Vec<DeliveryBlobReceiptV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct NodeCursor {
    pub incarnation_id: NodeIncarnationId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeHello {
    pub protocol_version: u16,
    pub incarnation_id: NodeIncarnationId,
    pub connection_id: u64,
    pub role: ClientRole,
    pub event_sequence: u64,
    pub controller: Option<ControllerState>,
    pub snapshot: NodeSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedNodeCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub request_id: u64,
    pub request: NodeRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NodeRequest {
    Snapshot,
    Resync { after_sequence: u64 },
    ArmHarnessMcpReservation {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        spawn_spec: SpawnSpec,
        expires_at_unix_ms: u64,
    },
    SpawnSpecWithHarnessMcp {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        spec: SpawnSpec,
        deadline_unix_ms: u64,
    },
    ActivateHarnessMcpReservation {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
    },
    AbortHarnessMcpReservation {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
    },
    PutHarnessMcpReplyChunk {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
        call_id: HarnessMcpCallId,
        offset: u32,
        final_chunk: bool,
        chunk_hex: HarnessMcpReplyChunkHexV1,
    },
    RejectHarnessMcpCall {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
        call_id: HarnessMcpCallId,
        reason: HarnessMcpRejectReasonV1,
    },
    BeginDeliveryStage {
        manifest: DeliveryBundleManifestV2,
    },
    PutDeliveryBlobChunk {
        stage_id: DeliveryStageId,
        blob_digest: DeliveryBlobDigestV1,
        offset: u64,
        chunk_hex: DeliveryBlobChunkHexV1,
    },
    CommitDeliveryStage {
        stage_id: DeliveryStageId,
    },
    AbortDeliveryStage {
        stage_id: DeliveryStageId,
    },
    BrowseHostDirectories {
        directory: Option<OpaqueHostPath>,
        after: Option<OpaqueHostPath>,
    },
    InspectWorkspace { workspace_id: WorkspaceId },
    ReadWorkspaceFile {
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    },
    WriteWorkspaceFile {
        workspace_id: WorkspaceId,
        path: RepositoryPath,
        expected_revision: WorkspaceFileRevision,
        #[serde(deserialize_with = "deserialize_workspace_file_text")]
        text: String,
    },
    CreateWorkspaceFile {
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    },
    CreateWorkspaceDirectory {
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    },
    ReadGitHistory {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<RepositoryPath>,
        before: Option<GitObjectId>,
        #[serde(deserialize_with = "deserialize_git_history_limit")]
        limit: u16,
    },
    ReadGitDiff {
        workspace_id: WorkspaceId,
        request: GitDiffRequest,
    },
    AcquireController { lease_ms: u64 },
    ReleaseController,
    RegisterWorkspace {
        workspace_id: WorkspaceId,
        root: OpaqueHostPath,
    },
    CreateStandaloneWorkspace {
        workspace_id: WorkspaceId,
        root: OpaqueHostPath,
        #[serde(default, deserialize_with = "deserialize_optional_initial_branch")]
        initial_branch: Option<String>,
    },
    UnregisterWorkspace {
        workspace_id: WorkspaceId,
    },
    CreateWorktree {
        source_workspace_id: WorkspaceId,
        workspace_id: WorkspaceId,
        target_root: OpaqueHostPath,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree {
        source_workspace_id: WorkspaceId,
        target_root: OpaqueHostPath,
    },
    Spawn {
        workspace_id: WorkspaceId,
        provider: AgentId,
        mode: SessionMode,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    SpawnSpec {
        spec: SpawnSpec,
    },
    SpawnManagedWorktree {
        request: ManagedWorktreeSpawnRequest,
    },
    SpawnManagedWorktreeV2 {
        request: ManagedWorktreeSpawnRequestV2,
    },
    CleanupManagedWorktree {
        lease_id: ManagedWorktreeLeaseId,
    },
    Resume {
        session: SessionAddress,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    RenameSessionRecord {
        record_id: SessionRecordId,
        display_name: String,
    },
    SetSessionTask {
        record_id: SessionRecordId,
        expected_revision: u64,
        target: SessionTaskTargetV1,
    },
    IndexProviderSession {
        workspace_id: WorkspaceId,
        provider: AgentId,
        identity: ProviderSessionIdentity,
        display_name: String,
    },
    IndexNativeSession {
        selection: NativeSessionSelection,
        display_name: String,
    },
    ResumeSessionRecord {
        record_id: SessionRecordId,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    ForgetSessionRecord {
        record_id: SessionRecordId,
    },
    CatalogNativeSessions {
        route: NativeSessionCatalogRoute,
        #[serde(deserialize_with = "deserialize_native_session_catalog_limit")]
        limit: u16,
    },
    PageNativeSessions {
        route: NativeSessionCatalogRoute,
        window: NativeSessionCatalogWindow,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        #[serde(default, deserialize_with = "deserialize_optional_history_candidate_id")]
        after_selection_id: Option<String>,
        #[serde(deserialize_with = "deserialize_native_session_catalog_limit")]
        limit: u16,
    },
    PreviewNativeSession {
        selection: NativeSessionSelection,
        #[serde(deserialize_with = "deserialize_native_session_preview_limit")]
        message_limit: u16,
    },
    PreviewSessionRecord {
        record_id: SessionRecordId,
        #[serde(deserialize_with = "deserialize_native_session_preview_limit")]
        message_limit: u16,
    },
    DiscoverHistory {
        session: SessionAddress,
        #[serde(deserialize_with = "deserialize_history_discovery_limit")]
        limit: u16,
    },
    LoadHistory {
        session: SessionAddress,
        #[serde(deserialize_with = "deserialize_history_candidate_id")]
        candidate_id: String,
    },
    ExportContextPack {
        session: SessionAddress,
    },
    ForgetContextPack {
        context_id: SpawnContextId,
    },
    Prompt { session: SessionAddress, text: String },
    Paste { session: SessionAddress, text: String },
    Input { session: SessionAddress, text: String },
    TerminalBytes { session: SessionAddress, bytes: Vec<u8> },
    TerminalControl { session: SessionAddress, control: TerminalControl },
    Resize { session: SessionAddress, size: TerminalSize },
    Interrupt { session: SessionAddress },
    Stop { session: SessionAddress, force: bool },
    Remove { session: SessionAddress },
    Shutdown,
}

fn deserialize_workspace_file_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    if text.len() > MAX_WORKSPACE_FILE_BYTES {
        return Err(serde::de::Error::custom("workspace file text exceeds the byte limit"));
    }
    Ok(text)
}

fn deserialize_git_history_limit<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let limit = u16::deserialize(deserializer)?;
    if !(1..=MAX_GIT_HISTORY_COMMITS).contains(&limit) {
        return Err(serde::de::Error::custom("git history limit is invalid"));
    }
    Ok(limit)
}

fn deserialize_optional_initial_branch<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let branch = Option::<String>::deserialize(deserializer)?;
    if let Some(branch) = branch.as_deref() {
        if branch.is_empty()
            || branch.len() > MAX_REPOSITORY_PATH_BYTES
            || branch.starts_with('-')
            || branch.chars().any(char::is_control)
        {
            return Err(serde::de::Error::custom(
                "initial branch must be bounded, non-empty, option-safe, and free of control characters",
            ));
        }
    }
    Ok(branch)
}

impl NodeRequest {
    pub fn harness_mcp_contract_is_valid_at(&self, now_unix_ms: u64) -> bool {
        match self {
            Self::ArmHarnessMcpReservation { expires_at_unix_ms, .. } => {
                *expires_at_unix_ms > now_unix_ms
                    && expires_at_unix_ms.saturating_sub(now_unix_ms)
                        <= MAX_HARNESS_MCP_RESERVATION_TTL_MS
            }
            Self::SpawnSpecWithHarnessMcp { deadline_unix_ms, .. } => {
                *deadline_unix_ms > now_unix_ms
                    && deadline_unix_ms.saturating_sub(now_unix_ms)
                        <= MAX_HARNESS_MCP_SPAWN_RELAY_DEADLINE_MS
            }
            Self::PutHarnessMcpReplyChunk { offset, chunk_hex, .. } => {
                usize::try_from(*offset).ok()
                    .and_then(|offset| offset.checked_add(chunk_hex.raw_len()))
                    .is_some_and(|end| end <= MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES)
            }
            _ => true,
        }
    }

    pub fn native_session_catalog_contract_is_valid(&self) -> bool {
        match self {
            Self::CatalogNativeSessions { route, limit } => {
                route.validate().is_ok()
                    && (1..=NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(limit)
            }
            Self::PageNativeSessions { route, after_selection_id, limit, .. } => {
                route.validate().is_ok()
                    && (1..=NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(limit)
                    && after_selection_id
                        .as_deref()
                        .map_or(true, |cursor| {
                            gate4agent_types::validate_candidate_id(cursor).is_ok()
                        })
            }
            _ => true,
        }
    }

    pub fn native_session_preview_contract_is_valid(&self) -> bool {
        match self {
            Self::PreviewNativeSession { selection, message_limit } => {
                (1..=NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX).contains(message_limit)
                    && selection.validate().is_ok()
            }
            Self::PreviewSessionRecord { message_limit, .. } => {
                (1..=NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX).contains(message_limit)
            }
            _ => true,
        }
    }

    pub fn history_context_pack_contract_is_valid(&self) -> bool {
        match self {
            Self::DiscoverHistory { limit, .. } => {
                (1..=HISTORY_DISCOVERY_LIMIT_MAX).contains(limit)
            }
            Self::LoadHistory { candidate_id, .. } => {
                gate4agent_types::validate_candidate_id(candidate_id).is_ok()
            }
            Self::Snapshot
            | Self::Resync { .. }
            | Self::ArmHarnessMcpReservation { .. }
            | Self::SpawnSpecWithHarnessMcp { .. }
            | Self::ActivateHarnessMcpReservation { .. }
            | Self::AbortHarnessMcpReservation { .. }
            | Self::PutHarnessMcpReplyChunk { .. }
            | Self::RejectHarnessMcpCall { .. }
            | Self::BeginDeliveryStage { .. }
            | Self::PutDeliveryBlobChunk { .. }
            | Self::CommitDeliveryStage { .. }
            | Self::AbortDeliveryStage { .. }
            | Self::BrowseHostDirectories { .. }
            | Self::InspectWorkspace { .. }
            | Self::ReadWorkspaceFile { .. }
            | Self::WriteWorkspaceFile { .. }
            | Self::CreateWorkspaceFile { .. }
            | Self::CreateWorkspaceDirectory { .. }
            | Self::ReadGitHistory { .. }
            | Self::ReadGitDiff { .. }
            | Self::AcquireController { .. }
            | Self::ReleaseController
            | Self::RegisterWorkspace { .. }
            | Self::CreateStandaloneWorkspace { .. }
            | Self::UnregisterWorkspace { .. }
            | Self::CreateWorktree { .. }
            | Self::RemoveWorktree { .. }
            | Self::Spawn { .. }
            | Self::SpawnSpec { .. }
            | Self::SpawnManagedWorktree { .. }
            | Self::SpawnManagedWorktreeV2 { .. }
            | Self::CleanupManagedWorktree { .. }
            | Self::Resume { .. }
            | Self::RenameSessionRecord { .. }
            | Self::SetSessionTask { .. }
            | Self::IndexProviderSession { .. }
            | Self::IndexNativeSession { .. }
            | Self::ResumeSessionRecord { .. }
            | Self::ForgetSessionRecord { .. }
            | Self::CatalogNativeSessions { .. }
            | Self::PageNativeSessions { .. }
            | Self::PreviewNativeSession { .. }
            | Self::PreviewSessionRecord { .. }
            | Self::ExportContextPack { .. }
            | Self::ForgetContextPack { .. }
            | Self::Prompt { .. }
            | Self::Paste { .. }
            | Self::Input { .. }
            | Self::TerminalBytes { .. }
            | Self::TerminalControl { .. }
            | Self::Resize { .. }
            | Self::Interrupt { .. }
            | Self::Stop { .. }
            | Self::Remove { .. }
            | Self::Shutdown => true,
        }
    }

    pub fn required_capability(&self) -> Option<&'static str> {
        match self {
            Self::ArmHarnessMcpReservation { .. }
            | Self::SpawnSpecWithHarnessMcp { .. }
            | Self::ActivateHarnessMcpReservation { .. }
            | Self::AbortHarnessMcpReservation { .. }
            | Self::PutHarnessMcpReplyChunk { .. }
            | Self::RejectHarnessMcpCall { .. } => {
                Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY)
            }
            Self::BeginDeliveryStage { .. }
            | Self::PutDeliveryBlobChunk { .. }
            | Self::CommitDeliveryStage { .. }
            | Self::AbortDeliveryStage { .. } => {
                Some(NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY)
            }
            Self::BrowseHostDirectories { .. } => Some(CAPABILITY_HOST_DIRECTORY_BROWSE_V1),
            Self::CreateStandaloneWorkspace { .. } => {
                Some(NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY)
            }
            Self::ReadWorkspaceFile { .. } => Some(NODE_WORKSPACE_FILE_READ_CAPABILITY),
            Self::WriteWorkspaceFile { .. } => Some(NODE_WORKSPACE_FILE_WRITE_CAPABILITY),
            Self::CreateWorkspaceFile { .. } | Self::CreateWorkspaceDirectory { .. } => {
                Some(NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY)
            }
            Self::ReadGitHistory { .. } | Self::ReadGitDiff { .. } => {
                Some(NODE_GIT_READ_CAPABILITY)
            }
            Self::IndexProviderSession { .. } => {
                Some(NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY)
            }
            Self::SetSessionTask { .. } => Some(NODE_SESSION_TASK_CORRELATION_CAPABILITY),
            Self::IndexNativeSession { .. } => Some(NODE_NATIVE_SESSION_INDEX_CAPABILITY),
            Self::CatalogNativeSessions { .. } => Some(NODE_NATIVE_SESSION_CATALOG_CAPABILITY),
            Self::PageNativeSessions { .. } => {
                Some(NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY)
            }
            Self::PreviewNativeSession { .. } => Some(NODE_NATIVE_SESSION_PREVIEW_CAPABILITY),
            Self::PreviewSessionRecord { .. } => Some(NODE_NATIVE_SESSION_PREVIEW_CAPABILITY),
            Self::SpawnSpec { .. } => Some(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY),
            Self::SpawnManagedWorktree { .. }
            | Self::CleanupManagedWorktree { .. } => {
                Some(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY)
            }
            Self::SpawnManagedWorktreeV2 { .. } => {
                Some(NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY)
            }
            Self::DiscoverHistory { .. }
            | Self::LoadHistory { .. }
            | Self::ExportContextPack { .. }
            | Self::ForgetContextPack { .. } => {
                Some(NODE_HISTORY_CONTEXT_PACK_CAPABILITY)
            }
            Self::Snapshot
            | Self::Resync { .. }
            | Self::InspectWorkspace { .. }
            | Self::AcquireController { .. }
            | Self::ReleaseController
            | Self::RegisterWorkspace { .. }
            | Self::UnregisterWorkspace { .. }
            | Self::CreateWorktree { .. }
            | Self::RemoveWorktree { .. }
            | Self::Spawn { .. }
            | Self::Resume { .. }
            | Self::RenameSessionRecord { .. }
            | Self::ResumeSessionRecord { .. }
            | Self::ForgetSessionRecord { .. }
            | Self::Prompt { .. }
            | Self::Paste { .. }
            | Self::Input { .. }
            | Self::TerminalBytes { .. }
            | Self::TerminalControl { .. }
            | Self::Resize { .. }
            | Self::Interrupt { .. }
            | Self::Stop { .. }
            | Self::Remove { .. }
            | Self::Shutdown => None,
        }
    }

    pub fn requires_worktree_selection_capability(&self) -> bool {
        matches!(self, Self::SpawnSpec { spec } if spec.target.worktree_id.is_some())
            || matches!(self,
                Self::ArmHarnessMcpReservation { spawn_spec: spec, .. }
                | Self::SpawnSpecWithHarnessMcp { spec, .. }
                if spec.target.worktree_id.is_some())
            || matches!(
                self,
                Self::SpawnManagedWorktree { .. } | Self::CleanupManagedWorktree { .. }
                    | Self::SpawnManagedWorktreeV2 { .. }
            )
    }

    pub fn requires_spawn_spec_defaults_overrides_capability(&self) -> bool {
        matches!(
            self,
            Self::SpawnSpec { .. }
                | Self::SpawnManagedWorktree { .. }
                | Self::SpawnManagedWorktreeV2 { .. }
                | Self::ArmHarnessMcpReservation { .. }
                | Self::SpawnSpecWithHarnessMcp { .. }
        )
    }

    pub fn requires_spawn_profile_revision_capability(&self) -> bool {
        matches!(
            self,
            Self::SpawnSpec { .. }
                | Self::SpawnManagedWorktree { .. }
                | Self::SpawnManagedWorktreeV2 { .. }
                | Self::ArmHarnessMcpReservation { .. }
                | Self::SpawnSpecWithHarnessMcp { .. }
        )
    }

    pub fn requires_child_environment_profile_capability(&self) -> bool {
        let spec = match self {
            Self::SpawnSpec { spec } => spec,
            Self::ArmHarnessMcpReservation { spawn_spec: spec, .. }
            | Self::SpawnSpecWithHarnessMcp { spec, .. } => spec,
            Self::SpawnManagedWorktree { request } => &request.spawn_spec,
            Self::SpawnManagedWorktreeV2 { request } => &request.spawn_spec,
            _ => return false,
        };
        matches!(
            spec.overrides.environment_profile_id,
            SpawnOverride::Set { .. }
        )
    }


    pub fn requires_session_bundle_materialization_capability(&self) -> bool {
        let spec = match self {
            Self::SpawnSpec { spec } => spec,
            Self::ArmHarnessMcpReservation { spawn_spec: spec, .. }
            | Self::SpawnSpecWithHarnessMcp { spec, .. } => spec,
            Self::SpawnManagedWorktree { request } => &request.spawn_spec,
            Self::SpawnManagedWorktreeV2 { request } => &request.spawn_spec,
            _ => return false,
        };
        !matches!(spec.overrides.bundle_id, SpawnOverride::Clear)
    }

    pub fn requires_history_context_pack_capability(&self) -> bool {
        match self {
            Self::DiscoverHistory { .. }
            | Self::LoadHistory { .. }
            | Self::ExportContextPack { .. }
            | Self::ForgetContextPack { .. } => true,
            Self::SpawnSpec { spec } => {
                !matches!(spec.overrides.context_id, SpawnOverride::Clear)
            }
            Self::ArmHarnessMcpReservation { spawn_spec: spec, .. }
            | Self::SpawnSpecWithHarnessMcp { spec, .. } => {
                !matches!(spec.overrides.context_id, SpawnOverride::Clear)
            }
            Self::SpawnManagedWorktree { request } => {
                !matches!(request.spawn_spec.overrides.context_id, SpawnOverride::Clear)
            }
            Self::SpawnManagedWorktreeV2 { request } => {
                !matches!(request.spawn_spec.overrides.context_id, SpawnOverride::Clear)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseEnvelope {
    pub request_id: u64,
    pub result: Result<NodeResponse, NodeFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeResponse {
    Snapshot {
        event_sequence: u64,
        controller: Option<ControllerState>,
        snapshot: NodeSnapshot,
    },
    Resync {
        event_sequence: u64,
        oldest_available_sequence: u64,
        snapshot: NodeSnapshot,
        events: Vec<NodeEventEnvelope>,
    },
    Armed {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        expires_at_unix_ms: u64,
    },
    Spawned {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        receipt: ResolvedSpawnReceipt,
    },
    Activated {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
    },
    Aborted {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
    },
    ReplyChunkAccepted {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
        call_id: HarnessMcpCallId,
        next_offset: u32,
        completed: bool,
    },
    CallRejected {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
        call_id: HarnessMcpCallId,
    },
    DeliveryStageBegun {
        stage_id: DeliveryStageId,
        manifest_digest: DeliveryManifestDigestV2,
        #[serde(deserialize_with = "deserialize_delivery_blob_digests")]
        missing_blobs: Vec<DeliveryBlobDigestV1>,
    },
    DeliveryBlobChunkAccepted {
        stage_id: DeliveryStageId,
        blob_digest: DeliveryBlobDigestV1,
        next_offset: u64,
    },
    DeliveryCommitted {
        receipt: DeliveryCommitReceiptV1,
    },
    DeliveryStageAborted {
        stage_id: DeliveryStageId,
    },
    WorkspaceInspected {
        inspection: WorkspaceInspection,
    },
    HostDirectoriesBrowsed {
        listing: HostDirectoryListing,
    },
    WorkspaceFileRead {
        file: WorkspaceFileRead,
    },
    WorkspaceFileWritten {
        file: WorkspaceFileRead,
    },
    WorkspaceFileCreated {
        file: WorkspaceFileRead,
    },
    WorkspaceDirectoryCreated {
        workspace_id: WorkspaceId,
        entry: WorkspaceEntry,
    },
    GitHistoryRead {
        workspace_id: WorkspaceId,
        page: GitHistoryPage,
    },
    GitDiffRead {
        workspace_id: WorkspaceId,
        diff: GitDiff,
    },
    Controller {
        controller: Option<ControllerState>,
    },
    SpawnAccepted {
        session: SessionAddress,
    },
    SpawnSpecAccepted {
        receipt: ResolvedSpawnReceipt,
    },
    ManagedWorktreeSpawnAccepted {
        receipt: ManagedWorktreeSpawnReceipt,
    },
    ManagedWorktreeCleanup {
        lease: ManagedWorktreeLeaseSnapshot,
    },
    SessionRecordUpdated {
        record: ManagedSessionRecord,
    },
    ProviderSessionIndexed {
        record: ManagedSessionRecord,
    },
    NativeSessionIndexed {
        selection: NativeSessionSelection,
        record: ManagedSessionRecord,
    },
    SessionRecordResumed {
        record: ManagedSessionRecord,
        session: SessionAddress,
    },
    SessionRecordForgotten {
        record_id: SessionRecordId,
    },
    NativeSessionsCataloged {
        route: NativeSessionCatalogRoute,
        #[serde(deserialize_with = "deserialize_native_session_catalog_entries")]
        entries: Vec<NativeSessionCatalogEntry>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_native_session_catalog_summary"
        )]
        summary: Option<NativeSessionCatalogSummary>,
    },
    NativeSessionsPaged {
        route: NativeSessionCatalogRoute,
        #[serde(deserialize_with = "deserialize_native_session_catalog_page")]
        page: NativeSessionCatalogPage,
    },
    NativeSessionPreviewed {
        selection: NativeSessionSelection,
        #[serde(deserialize_with = "deserialize_native_session_preview")]
        preview: NativeSessionPreview,
    },
    SessionRecordPreviewed {
        record_id: SessionRecordId,
        #[serde(deserialize_with = "deserialize_session_record_preview")]
        preview: SessionRecordPreview,
    },
    HistoryDiscovered {
        session: SessionAddress,
        #[serde(deserialize_with = "deserialize_history_candidates")]
        candidates: Vec<HistoryCandidateSummary>,
    },
    HistoryLoaded {
        session: SessionAddress,
        #[serde(deserialize_with = "deserialize_history_session_id")]
        session_id: String,
        message_count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completed_turn_count: Option<u64>,
    },
    ContextPackExported {
        context: ResolvedContextPackReceipt,
    },
    ContextPackForgotten {
        context_id: SpawnContextId,
    },
    WorkspaceRegistered {
        workspace: WorkspaceSnapshot,
    },
    StandaloneWorkspaceCreated {
        workspace: WorkspaceSnapshot,
    },
    WorkspaceUnregistered {
        workspace_id: WorkspaceId,
    },
    WorktreeCreated {
        worktree: GitWorktreeSnapshot,
        workspace: WorkspaceSnapshot,
    },
    WorktreeRemoved {
        target_root: OpaqueHostPath,
        workspace_id: Option<WorkspaceId>,
    },
    Accepted,
    ShuttingDown,
}

impl NodeResponse {
    pub fn requires_harness_mcp_proxy_capability(&self) -> bool {
        matches!(self,
            Self::Armed { .. }
                | Self::Spawned { .. }
                | Self::Activated { .. }
                | Self::Aborted { .. }
                | Self::ReplyChunkAccepted { .. }
                | Self::CallRejected { .. })
            || matches!(self, Self::SpawnSpecAccepted { receipt }
                if receipt.harness_mcp_proxy.is_some())
            || matches!(self, Self::ManagedWorktreeSpawnAccepted { receipt }
                if receipt.spawn.harness_mcp_proxy.is_some())
    }

    pub fn native_session_catalog_contract_is_valid(&self) -> bool {
        match self {
            Self::NativeSessionsCataloged { route, entries, summary: Some(summary) } => {
                route.validate().is_ok()
                    && validate_native_session_catalog_entries(route, entries).is_ok()
                    && summary.validate_initial_entries(entries.len()).is_ok()
            }
            Self::NativeSessionsCataloged { route, entries, summary: None } => {
                route.validate().is_ok()
                    && validate_native_session_catalog_entries(route, entries).is_ok()
            }
            Self::NativeSessionsPaged { route, page } => {
                page.validate_for_route(route).is_ok()
            }
            _ => true,
        }
    }

    pub fn requires_native_session_catalog_capability(&self) -> bool {
        matches!(self, Self::NativeSessionsCataloged { .. })
    }

    pub fn requires_native_session_catalog_paging_capability(&self) -> bool {
        matches!(self, Self::NativeSessionsPaged { .. })
    }

    pub fn requires_native_session_preview_capability(&self) -> bool {
        matches!(
            self,
            Self::NativeSessionPreviewed { .. } | Self::SessionRecordPreviewed { .. }
        )
    }

    pub fn requires_native_session_index_capability(&self) -> bool {
        matches!(self, Self::NativeSessionIndexed { .. })
    }

    pub fn native_session_index_contract_is_valid(&self) -> bool {
        match self {
            Self::NativeSessionIndexed { selection, record } => {
                selection.validate().is_ok()
                    && selection.route.scope == NativeSessionCatalogScope::Workspace
                    && selection.route.workspace_id.as_ref() == Some(&record.workspace_id)
                    && selection.route.provider == record.provider
            }
            _ => false,
        }
    }

    pub fn native_session_preview_contract_is_valid(&self) -> bool {
        match self {
            Self::NativeSessionPreviewed { selection, preview } => {
                selection.validate().is_ok() && preview.validate().is_ok()
            }
            Self::SessionRecordPreviewed { preview, .. } => preview.validate().is_ok(),
            _ => true,
        }
    }

    pub fn requires_worktree_selection_capability(&self) -> bool {
        matches!(self,
            Self::SpawnSpecAccepted { receipt }
                | Self::Spawned { receipt, .. }
            if receipt.target.worktree_id.is_some())
            || matches!(
                self,
                Self::ManagedWorktreeSpawnAccepted { .. }
                    | Self::ManagedWorktreeCleanup { .. }
            )
    }


    pub fn requires_spawn_spec_defaults_overrides_capability(&self) -> bool {
        matches!(
            self,
            Self::SpawnSpecAccepted { .. }
                | Self::Spawned { .. }
                | Self::ManagedWorktreeSpawnAccepted { .. }
        )
    }

    pub fn requires_spawn_profile_revision_capability(&self) -> bool {
        matches!(
            self,
            Self::SpawnSpecAccepted { .. }
                | Self::Spawned { .. }
                | Self::ManagedWorktreeSpawnAccepted { .. }
        )
    }

    pub fn requires_child_environment_profile_capability(&self) -> bool {
        match self {
            Self::Snapshot { snapshot, .. } => {
                snapshot.requires_child_environment_profile_capability()
            }
            Self::Resync {
                snapshot, events, ..
            } => {
                snapshot.requires_child_environment_profile_capability()
                    || events.iter().any(|event| {
                        event.event.requires_child_environment_profile_capability()
                    })
            }
            Self::SpawnSpecAccepted { receipt }
            | Self::Spawned { receipt, .. } => receipt.environment_profile.is_some(),
            Self::ManagedWorktreeSpawnAccepted { receipt } => {
                receipt.spawn.environment_profile.is_some()
            }
            Self::SessionRecordUpdated { record }
            | Self::ProviderSessionIndexed { record }
            | Self::NativeSessionIndexed { record, .. }
            | Self::SessionRecordResumed { record, .. } => {
                record.environment_profile.is_some()
            }
            Self::Armed { .. }
            | Self::Activated { .. }
            | Self::Aborted { .. }
            | Self::ReplyChunkAccepted { .. }
            | Self::CallRejected { .. }
            | Self::WorkspaceInspected { .. }
            | Self::DeliveryStageBegun { .. }
            | Self::DeliveryBlobChunkAccepted { .. }
            | Self::DeliveryCommitted { .. }
            | Self::DeliveryStageAborted { .. }
            | Self::HostDirectoriesBrowsed { .. }
            | Self::WorkspaceFileRead { .. }
            | Self::WorkspaceFileWritten { .. }
            | Self::WorkspaceFileCreated { .. }
            | Self::WorkspaceDirectoryCreated { .. }
            | Self::GitHistoryRead { .. }
            | Self::GitDiffRead { .. }
            | Self::Controller { .. }
            | Self::SpawnAccepted { .. }
            | Self::ManagedWorktreeCleanup { .. }
            | Self::SessionRecordForgotten { .. }
            | Self::NativeSessionsCataloged { .. }
            | Self::NativeSessionsPaged { .. }
            | Self::NativeSessionPreviewed { .. }
            | Self::SessionRecordPreviewed { .. }
            | Self::HistoryDiscovered { .. }
            | Self::HistoryLoaded { .. }
            | Self::ContextPackExported { .. }
            | Self::ContextPackForgotten { .. }
            | Self::WorkspaceRegistered { .. }
            | Self::StandaloneWorkspaceCreated { .. }
            | Self::WorkspaceUnregistered { .. }
            | Self::WorktreeCreated { .. }
            | Self::WorktreeRemoved { .. }
            | Self::Accepted
            | Self::ShuttingDown => false,
        }
    }

    pub fn requires_session_bundle_materialization_capability(&self) -> bool {
        match self {
            Self::Snapshot { snapshot, .. } => {
                snapshot.requires_session_bundle_materialization_capability()
            }
            Self::Resync {
                snapshot, events, ..
            } => {
                snapshot.requires_session_bundle_materialization_capability()
                    || events.iter().any(|event| {
                        event.event.requires_session_bundle_materialization_capability()
                    })
            }
            Self::SpawnSpecAccepted { receipt }
            | Self::Spawned { receipt, .. } => receipt.bundle.is_some(),
            Self::ManagedWorktreeSpawnAccepted { receipt } => receipt.spawn.bundle.is_some(),
            Self::SessionRecordUpdated { record }
            | Self::ProviderSessionIndexed { record }
            | Self::NativeSessionIndexed { record, .. }
            | Self::SessionRecordResumed { record, .. } => record.bundle.is_some(),
            Self::Armed { .. }
            | Self::Activated { .. }
            | Self::Aborted { .. }
            | Self::ReplyChunkAccepted { .. }
            | Self::CallRejected { .. }
            | Self::WorkspaceInspected { .. }
            | Self::DeliveryStageBegun { .. }
            | Self::DeliveryBlobChunkAccepted { .. }
            | Self::DeliveryCommitted { .. }
            | Self::DeliveryStageAborted { .. }
            | Self::HostDirectoriesBrowsed { .. }
            | Self::WorkspaceFileRead { .. }
            | Self::WorkspaceFileWritten { .. }
            | Self::WorkspaceFileCreated { .. }
            | Self::WorkspaceDirectoryCreated { .. }
            | Self::GitHistoryRead { .. }
            | Self::GitDiffRead { .. }
            | Self::Controller { .. }
            | Self::SpawnAccepted { .. }
            | Self::ManagedWorktreeCleanup { .. }
            | Self::SessionRecordForgotten { .. }
            | Self::NativeSessionsCataloged { .. }
            | Self::NativeSessionsPaged { .. }
            | Self::NativeSessionPreviewed { .. }
            | Self::SessionRecordPreviewed { .. }
            | Self::HistoryDiscovered { .. }
            | Self::HistoryLoaded { .. }
            | Self::ContextPackExported { .. }
            | Self::ContextPackForgotten { .. }
            | Self::WorkspaceRegistered { .. }
            | Self::StandaloneWorkspaceCreated { .. }
            | Self::WorkspaceUnregistered { .. }
            | Self::WorktreeCreated { .. }
            | Self::WorktreeRemoved { .. }
            | Self::Accepted
            | Self::ShuttingDown => false,
        }
    }

    pub fn requires_history_context_pack_capability(&self) -> bool {
        match self {
            Self::Snapshot { snapshot, .. } => {
                snapshot.requires_history_context_pack_capability()
            }
            Self::Resync {
                snapshot, events, ..
            } => {
                snapshot.requires_history_context_pack_capability()
                    || events.iter().any(|event| {
                        event.event.requires_history_context_pack_capability()
                    })
            }
            Self::SpawnSpecAccepted { receipt }
            | Self::Spawned { receipt, .. } => {
                receipt.context_id.is_some() || receipt.context.is_some()
            }
            Self::ManagedWorktreeSpawnAccepted { receipt } => {
                receipt.spawn.context_id.is_some() || receipt.spawn.context.is_some()
            }
            Self::SessionRecordUpdated { record }
            | Self::ProviderSessionIndexed { record }
            | Self::NativeSessionIndexed { record, .. }
            | Self::SessionRecordResumed { record, .. } => {
                record.context_id.is_some() || record.context.is_some()
            }
            Self::HistoryDiscovered { .. }
            | Self::HistoryLoaded { .. }
            | Self::ContextPackExported { .. }
            | Self::ContextPackForgotten { .. } => true,
            Self::Armed { .. }
            | Self::Activated { .. }
            | Self::Aborted { .. }
            | Self::ReplyChunkAccepted { .. }
            | Self::CallRejected { .. }
            | Self::WorkspaceInspected { .. }
            | Self::DeliveryStageBegun { .. }
            | Self::DeliveryBlobChunkAccepted { .. }
            | Self::DeliveryCommitted { .. }
            | Self::DeliveryStageAborted { .. }
            | Self::HostDirectoriesBrowsed { .. }
            | Self::WorkspaceFileRead { .. }
            | Self::WorkspaceFileWritten { .. }
            | Self::WorkspaceFileCreated { .. }
            | Self::WorkspaceDirectoryCreated { .. }
            | Self::GitHistoryRead { .. }
            | Self::GitDiffRead { .. }
            | Self::Controller { .. }
            | Self::SpawnAccepted { .. }
            | Self::ManagedWorktreeCleanup { .. }
            | Self::SessionRecordForgotten { .. }
            | Self::NativeSessionsCataloged { .. }
            | Self::NativeSessionsPaged { .. }
            | Self::NativeSessionPreviewed { .. }
            | Self::SessionRecordPreviewed { .. }
            | Self::WorkspaceRegistered { .. }
            | Self::StandaloneWorkspaceCreated { .. }
            | Self::WorkspaceUnregistered { .. }
            | Self::WorktreeCreated { .. }
            | Self::WorktreeRemoved { .. }
            | Self::Accepted
            | Self::ShuttingDown => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeFailure {
    pub code: NodeFailureCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeFailureCode {
    InvalidRequest,
    UnsupportedCapability,
    SpawnProfileRevisionMismatch,
    HarnessMcpUnavailable,
    ReservationNotFound,
    ReservationConflict,
    ReservationExpired,
    BindingMismatch,
    NotActivated,
    CallNotFound,
    ChunkOutOfOrder,
    ResponseTooLarge,
    DeliveryManifestInvalid,
    UnknownDeliveryStage,
    DeliveryStageConflict,
    DeliveryBlobUnexpected,
    DeliveryChunkOutOfOrder,
    DeliveryBlobDigestMismatch,
    DeliveryBundleDigestMismatch,
    DeliveryStageIncomplete,
    DeliveryStageStorageFailed,
    Unauthorized,
    ObserverReadOnly,
    ControllerBusy,
    ControllerRequired,
    UnknownWorkspace,
    HostDirectoryInvalid,
    HostDirectoryReadFailed,
    HostDirectoryReadTimedOut,
    InvalidRepositoryPath,
    RepositoryFileNotFound,
    RepositoryFileNotRegular,
    RepositoryPathUnsafe,
    RepositoryFileReadTimedOut,
    RepositoryFileReadFailed,
    RepositoryFileWriteTimedOut,
    RepositoryFileWriteFailed,
    RepositoryFileRevisionConflict,
    RepositoryEntryAlreadyExists,
    RepositoryParentNotFound,
    RepositoryParentNotDirectory,
    RepositoryEntryCreateTimedOut,
    RepositoryEntryCreateFailed,
    GitReadTimedOut,
    GitReadFailed,
    InvalidWorkspaceRoot,
    DuplicateWorkspaceId,
    DuplicateWorkspaceRoot,
    WorkspaceBusy,
    LastWorkspace,
    NotGitRepository,
    WorktreeConflict,
    WorktreeProtected,
    WorktreeDirty,
    WorktreeLocked,
    UnknownManagedWorktreeLease,
    ManagedWorktreeBusy,
    ManagedWorktreeOwnershipConflict,
    ManagedWorktreeProfileRevisionMismatch,
    ManagedWorktreeRecoveryRequired,
    StandaloneWorkspaceRecoveryRequired,
    UnknownSpawnProfile,
    UnknownBundle,
    UnknownContextPack,
    ContextPackBusy,
    ContextPackMaterializationFailed,
    UnknownEnvironmentProfile,
    BundleBindingMismatch,
    EnvironmentProfileBindingMismatch,
    BundleMaterializationFailed,
    SpawnTargetMismatch,
    SpawnIdempotencyConflict,
    SpawnIdempotencyCapacity,
    SpawnDeadlineExceeded,
    UnsupportedSpawnCapability,
    UnknownSession,
    UnknownSessionRecord,
    SessionRecordNotResumable,
    SessionRecordBusy,
    SessionRecordConflict,
    SessionWorkspaceMismatch,
    WorkspaceRegistrationRequired,
    StaleNativeSessionCatalog,
    StaleGeneration,
    BackendBusy,
    BackendDisconnected,
    BackendOperationFailed,
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeEventEnvelope {
    pub sequence: u64,
    pub event: NodeEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeEvent {
    HarnessMcpReadCall {
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        record_id: SessionRecordId,
        session: SessionAddress,
        call_id: HarnessMcpCallId,
        request: HarnessReadRequestV1,
        deadline_unix_ms: u64,
    },
    Control { address: SessionAddress, event: ControlEvent },
    Observation { address: SessionAddress, observation: ObservationV1 },
    ManagedObservation { record_id: SessionRecordId, observation: ObservationV1 },
    TerminalFrame { address: SessionAddress, frame: TerminalFrame },
    ControllerChanged { controller: Option<ControllerState> },
    WorkspaceAdded { workspace: WorkspaceSnapshot },
    WorkspaceRemoved { workspace_id: WorkspaceId },
    SessionRecordUpserted { record: ManagedSessionRecord },
    SessionRecordRemoved { record_id: SessionRecordId },
    ManagedWorktreeUpserted { lease: ManagedWorktreeLeaseSnapshot },
    ManagedWorktreeRemoved { lease_id: ManagedWorktreeLeaseId },
    ResyncRequired { oldest_available_sequence: u64 },
}

impl NodeEvent {
    pub fn requires_harness_mcp_proxy_capability(&self) -> bool {
        matches!(self, Self::HarnessMcpReadCall { .. })
    }

    pub fn harness_mcp_contract_is_valid_at(&self, now_unix_ms: u64) -> bool {
        match self {
            Self::HarnessMcpReadCall { request, deadline_unix_ms, .. } => {
                request.validate().is_ok()
                    && serde_json::to_vec(request)
                        .is_ok_and(|wire| wire.len() <= MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES)
                    && *deadline_unix_ms > now_unix_ms
                    && deadline_unix_ms.saturating_sub(now_unix_ms)
                        <= MAX_HARNESS_MCP_CALL_DEADLINE_MS
            }
            _ => true,
        }
    }
    pub fn requires_observation_events_capability(&self) -> bool {
        matches!(self, Self::Observation { .. } | Self::ManagedObservation { .. })
    }

    pub fn requires_observation_managed_target_capability(&self) -> bool {
        matches!(self, Self::ManagedObservation { .. })
    }

    pub fn requires_observation_workflow_detail_capability(&self) -> bool {
        matches!(self,
            Self::Observation { observation, .. }
            | Self::ManagedObservation { observation, .. }
            if observation.kind.requires_workflow_detail_capability())
    }

    pub fn requires_child_environment_profile_capability(&self) -> bool {
        matches!(self, Self::SessionRecordUpserted { record }
            if record.environment_profile.is_some())
    }


    pub fn requires_session_bundle_materialization_capability(&self) -> bool {
        matches!(self, Self::SessionRecordUpserted { record }
            if record.bundle.is_some())
    }

    pub fn requires_history_context_pack_capability(&self) -> bool {
        matches!(self, Self::SessionRecordUpserted { record }
            if record.context_id.is_some() || record.context.is_some())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum ClientFrame {
    Hello(ClientHello),
    Authenticate(ClientAuthentication),
    Request(RequestEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum ServerFrame {
    Challenge(ServerChallenge),
    Hello(NodeHello),
    Reply(ResponseEnvelope),
    Event(NodeEventEnvelope),
}

pub async fn read_json_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_json_frame_limited(reader, MAX_NODE_FRAME_BYTES).await
}

pub async fn read_json_frame_limited<R, T>(reader: &mut R, max_bytes: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32_le().await? as usize;
    if length == 0 || length > max_bytes {
        return Err(FrameError::InvalidLength {
            length,
            max: max_bytes,
        });
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

pub async fn read_json_frame_limited_body_timeout<R, T>(
    reader: &mut R,
    max_bytes: usize,
    body_timeout: Duration,
) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length_prefix = [0_u8; std::mem::size_of::<u32>()];
    reader.read_exact(&mut length_prefix[..1]).await?;
    timeout(body_timeout, reader.read_exact(&mut length_prefix[1..]))
        .await
        .map_err(|_| FrameError::PrefixTimedOut)??;
    let length = u32::from_le_bytes(length_prefix) as usize;
    if length == 0 || length > max_bytes {
        return Err(FrameError::InvalidLength {
            length,
            max: max_bytes,
        });
    }
    let mut payload = vec![0; length];
    timeout(body_timeout, reader.read_exact(&mut payload))
        .await
        .map_err(|_| FrameError::BodyTimedOut { length })??;
    Ok(serde_json::from_slice(&payload)?)
}

pub async fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_json_frame_limited(writer, value, MAX_NODE_FRAME_BYTES).await
}

pub async fn write_json_frame_limited<W, T>(
    writer: &mut W,
    value: &T,
    max_bytes: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() || payload.len() > max_bytes {
        return Err(FrameError::InvalidLength {
            length: payload.len(),
            max: max_bytes,
        });
    }
    writer.write_u32_le(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("node frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("node frame JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("node frame length {length} is outside 1..={max}")]
    InvalidLength { length: usize, max: usize },
    #[error("node frame body of {length} bytes was not received before the bounded deadline")]
    BodyTimedOut { length: usize },
    #[error("node frame length prefix was not completed before the bounded deadline")]
    PrefixTimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_mcp_protocol_serde_bounds_and_privacy() {
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}", "a".repeat(24),
        )).unwrap();
        let call_id = HarnessMcpCallId::new(format!("hmcpcall_{}", "b".repeat(24))).unwrap();
        let digest = HarnessMcpActivationDigest::new(format!(
            "sha256:{}", "c".repeat(64),
        )).unwrap();
        let token = HarnessMcpLocalToken::new(format!("g4ah3_{}", "d".repeat(64))).unwrap();
        assert!(!format!("{token:?}").contains(token.expose()));
        assert!(HarnessMcpReservationId::new(format!("hmcpres_{}", "A".repeat(24))).is_err());
        assert!(HarnessMcpCallId::new(format!("hmcpcall_{}", "b".repeat(23))).is_err());
        assert!(HarnessMcpActivationDigest::new(format!("sha256:{}", "g".repeat(64))).is_err());

        let local = HarnessMcpLocalRequestV1 {
            version: 1,
            token,
            request: HarnessReadRequestV1::ContextGet,
        };
        local.validate().unwrap();
        let mut unknown = serde_json::to_value(&local).unwrap();
        unknown["endpoint"] = serde_json::Value::String("forbidden".to_owned());
        assert!(serde_json::from_value::<HarnessMcpLocalRequestV1>(unknown).is_err());
        assert!(HarnessMcpReplyChunkHexV1::new(
            "00".repeat(MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES),
        ).is_ok());
        assert!(HarnessMcpReplyChunkHexV1::new(
            "00".repeat(MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES + 1),
        ).is_err());

        let event = NodeEvent::HarnessMcpReadCall {
            reservation_id: reservation_id.clone(),
            activation_digest: digest.clone(),
            record_id: SessionRecordId::new("record-a").unwrap(),
            session: session_address("primary", 1),
            call_id,
            request: HarnessReadRequestV1::ContextGet,
            deadline_unix_ms: 4_000,
        };
        assert!(event.harness_mcp_contract_is_valid_at(1_000));
        assert!(!event.harness_mcp_contract_is_valid_at(999));
        let request = NodeRequest::AbortHarnessMcpReservation {
            reservation_id,
            activation_digest: digest,
        };
        assert_eq!(request.required_capability(), Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY));
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("g4ah3_"));
        assert!(!json.contains("endpoint"));
        assert!(!json.contains("path"));
    }

    #[test]
    fn task_id_is_fixed_opaque_and_bounded() {
        let task_id = TaskId::from_nonce([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xff,
        ]);
        assert_eq!(task_id.as_str(), "task-00112233445566778899aaff");
        assert_eq!(task_id.as_str().len(), 29);
        assert_eq!(task_id.as_str().parse::<TaskId>().unwrap(), task_id);
        assert!("task-00112233445566778899aaf".parse::<TaskId>().is_err());
        assert!("task-00112233445566778899aaff00".parse::<TaskId>().is_err());
        assert!("task-00112233445566778899aaFF".parse::<TaskId>().is_err());
        assert!("work-00112233445566778899aaff".parse::<TaskId>().is_err());
        assert!(serde_json::from_str::<TaskId>(
            r#""task-00112233445566778899aaff-extra""#,
        )
        .is_err());
    }

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn host_path(value: &str) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.to_owned()).unwrap()
    }

    fn repository_path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
    }

    fn session_address(workspace_id: &str, instance_id: u64) -> SessionAddress {
        SessionAddress {
            workspace_id: WorkspaceId::new(workspace_id).unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(instance_id),
                generation: SessionGeneration(1),
            },
        }
    }

    fn context_receipt(
        id: &str,
        source_session: SessionAddress,
    ) -> ResolvedContextPackReceipt {
        ResolvedContextPackReceipt {
            id: SpawnContextId::new(id).unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-a").unwrap(),
                source_session,
                source_provider: agent("claude"),
            },
            source_message_count: 3,
            retained_message_count: 2,
            byte_len: 32,
            truncated: true,
        }
    }

    fn portable_node_support() -> NodeCompatibilitySupport {
        NodeCompatibilitySupport {
            protocol_versions: ProtocolRange::new(7, 9).unwrap(),
            capabilities: vec![
                CapabilityId::new("workspace.inspect").unwrap(),
                CapabilityId::new("session.spawn").unwrap(),
            ],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
            local_transport: LocalTransportKind::WindowsNamedPipe,
            state_schema: StateSchemaSupport {
                versions: ProtocolRange::new(3, 5).unwrap(),
            },
            provider_contracts: vec![ProviderContractSupport {
                provider: agent("codex"),
                revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
            }],
            provider_adapter_contracts: vec![ProviderAdapterContractSupport {
                provider: agent("codex"),
                family: AdapterFamily::PtySemantic,
                adapter_id: AdapterId::new("codex-cli").unwrap(),
                revision: AdapterContractRevision::new("pty-semantic-v1").unwrap(),
            }],
        }
    }

    #[test]
    fn legacy_hello_json_remains_exactly_protocol_v9() {
        let client = ClientHello::new(ClientRole::Observer, [0; NODE_AUTH_NONCE_BYTES]);
        assert_eq!(
            serde_json::to_string(&client).unwrap(),
            r#"{"protocol_version":9,"role":"observer","client_nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#,
        );

        let challenge = ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce: [0; NODE_AUTH_NONCE_BYTES],
            server_proof: [0; NODE_AUTH_PROOF_BYTES],
            compatibility: None,
        };
        let json = serde_json::to_string(&challenge).unwrap();
        assert!(!json.contains("compatibility"));
        assert_eq!(serde_json::from_str::<ServerChallenge>(&json).unwrap(), challenge);
    }

    #[test]
    fn legacy_hello_omits_compatibility_instead_of_synthesizing_a_selection() {
        let hello = ClientHello::new(ClientRole::Observer, [0; NODE_AUTH_NONCE_BYTES]);
        assert_eq!(hello.compatibility, None);
    }

    #[test]
    fn legacy_spawn_json_remains_exact_after_spawn_spec() {
        let request = NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            initial_prompt: None,
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"kind":"spawn","workspace_id":"primary","provider":"claude","mode":"pty","terminal_size":{"rows":24,"columns":80},"initial_prompt":null}"#,
        );
        assert_eq!(request.required_capability(), None);

        let response = NodeResponse::SpawnAccepted {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"kind":"spawn-accepted","session":{"workspace_id":"primary","session":{"instance_id":7,"generation":1}}}"#,
        );
    }

    #[test]
    fn history_loaded_completed_turn_count_is_optional_wire_metadata() {
        let legacy_json = r#"{"kind":"history-loaded","session":{"workspace_id":"primary","session":{"instance_id":7,"generation":1}},"session_id":"session-7","message_count":12}"#;
        let legacy = serde_json::from_str::<NodeResponse>(legacy_json).unwrap();
        assert_eq!(serde_json::to_string(&legacy).unwrap(), legacy_json);
        assert!(matches!(
            legacy,
            NodeResponse::HistoryLoaded {
                completed_turn_count: None,
                ..
            }
        ));

        let current = NodeResponse::HistoryLoaded {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
            session_id: "session-7".to_owned(),
            message_count: 12,
            completed_turn_count: Some(5),
        };
        assert_eq!(
            serde_json::to_string(&current).unwrap(),
            r#"{"kind":"history-loaded","session":{"workspace_id":"primary","session":{"instance_id":7,"generation":1}},"session_id":"session-7","message_count":12,"completed_turn_count":5}"#,
        );
    }

    #[test]
    fn spawn_spec_defaults_overrides_are_deterministic() {
        let profile_id = SpawnProfileId::new("review-default").unwrap();
        let defaults = SpawnProfileDefaults {
            profile_id: profile_id.clone(),
            revision: SpawnProfileRevision::new("review-default.r3").unwrap(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            prompt: Some(SpawnPrompt::new("profile prompt").unwrap()),
            bundle_id: Some(SpawnBundleId::new("review-bundle").unwrap()),
            context_id: Some(SpawnContextId::new("repo-context").unwrap()),
            environment_profile_id: Some(
                SpawnEnvironmentProfileId::new("local-default").unwrap(),
            ),
        };
        let spec = SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("primary").unwrap(),
                worktree_id: Some(WorkspaceId::new("review-tree").unwrap()),
            },
            profile_id,
            expected_profile_revision: defaults.revision.clone(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Inherit,
                mode: SpawnOverride::Set {
                    value: SessionMode::Inline,
                },
                terminal_size: SpawnOverride::Set {
                    value: TerminalSize {
                        rows: 31,
                        columns: 97,
                    },
                },
                prompt: SpawnOverride::Set {
                    value: SpawnPrompt::new("private prompt text").unwrap(),
                },
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Inherit,
                environment_profile_id: SpawnOverride::Clear,
            },
            deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("request-0001").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([
                CapabilityId::new(SPAWN_RUNTIME_STRUCTURED_PROMPT).unwrap(),
                CapabilityId::new(SPAWN_RUNTIME_RAW_PTY_LIFECYCLE).unwrap(),
            ])
            .unwrap(),
        };

        let first = spec.resolve(&defaults).unwrap();
        let second = spec.resolve(&defaults).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.provider, agent("claude"));
        assert_eq!(first.mode, SessionMode::Inline);
        assert_eq!(first.terminal_size.rows, 31);
        assert_eq!(first.prompt.as_ref().unwrap().as_str(), "private prompt text");
        assert_eq!(first.bundle_id, None);
        assert_eq!(
            first.context_id.as_ref().map(SpawnContextId::as_str),
            Some("repo-context"),
        );
        assert_eq!(first.environment_profile_id, None);
        assert_eq!(first.provenance.provider, SpawnFieldProvenance::Profile);
        assert_eq!(first.provenance.mode, SpawnFieldProvenance::Override);
        assert_eq!(first.provenance.prompt, SpawnFieldProvenance::Override);
        assert_eq!(first.provenance.bundle_id, SpawnFieldProvenance::Cleared);
        assert_eq!(first.provenance.context_id, SpawnFieldProvenance::Profile);
        assert_eq!(
            first.required_capabilities.as_slice(),
            &[
                CapabilityId::new(SPAWN_RUNTIME_RAW_PTY_LIFECYCLE).unwrap(),
                CapabilityId::new(SPAWN_RUNTIME_STRUCTURED_PROMPT).unwrap(),
            ],
        );

        let source_session = session_address("primary", 7);
        let receipt = first.receipt_with_materialization(
            NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            session_address("review-tree", 11),
            None,
            None,
            Some(context_receipt("repo-context", source_session)),
        );
        assert_eq!(receipt.prompt, SpawnPromptMetadata {
            present: true,
            byte_len: 19,
        });
        let receipt_json = serde_json::to_string(&receipt).unwrap();
        assert_eq!(
            receipt_json,
            r#"{"incarnation_id":"09090909090909090909090909090909","session":{"workspace_id":"review-tree","session":{"instance_id":11,"generation":1}},"target":{"node_id":"node-a","workspace_id":"primary","worktree_id":"review-tree"},"profile_id":"review-default","profile_revision":"review-default.r3","provider":"claude","mode":"inline","terminal_size":{"rows":31,"columns":97},"prompt":{"present":true,"byte_len":19},"bundle_id":null,"context_id":"repo-context","context":{"id":"repo-context","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","lineage":{"source_node_id":"node-a","source_session":{"workspace_id":"primary","session":{"instance_id":7,"generation":1}},"source_provider":"claude"},"source_message_count":3,"retained_message_count":2,"byte_len":32,"truncated":true},"environment_profile_id":null,"deadline_ms":30000,"idempotency_key":"request-0001","required_capabilities":["raw-pty-lifecycle","structured-prompt"],"provenance":{"provider":"profile","mode":"override","terminal_size":"override","prompt":"override","bundle_id":"cleared","context_id":"profile","environment_profile_id":"cleared"}}"#,
        );
        assert_eq!(
            serde_json::from_str::<ResolvedSpawnReceipt>(&receipt_json).unwrap(),
            receipt,
        );
        assert!(!receipt_json.contains("private prompt text"));
        assert!(!receipt_json.contains("profile prompt"));

        let environment_profile = ResolvedEnvironmentProfileReceipt {
            profile_id: SpawnEnvironmentProfileId::new("local-default").unwrap(),
            profile_revision: SpawnEnvironmentProfileRevision::new(
                "local-default.2026-08",
            )
            .unwrap(),
        };
        let environment_receipt = first.receipt_with_materialization(
            NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            receipt.session.clone(),
            Some(environment_profile.clone()),
            None,
            receipt.context.clone(),
        );
        let environment_json = serde_json::to_string(&environment_receipt).unwrap();
        assert!(environment_json.contains(
            r#""environment_profile_id":{"profile_id":"local-default","profile_revision":"local-default.2026-08"}"#,
        ));
        assert_eq!(
            serde_json::from_str::<ResolvedSpawnReceipt>(&environment_json)
                .unwrap()
                .environment_profile,
            Some(environment_profile),
        );
        assert!(serde_json::from_str::<ResolvedEnvironmentProfileReceipt>(
            r#"{"profile_id":"local-default","profile_revision":"r1","value":"secret"}"#,
        )
        .is_err());
        assert!(SpawnEnvironmentProfileRevision::new(
            "r".repeat(MAX_SPAWN_ENVIRONMENT_PROFILE_REVISION_BYTES),
        )
        .is_ok());
        assert!(SpawnEnvironmentProfileRevision::new(
            "r".repeat(MAX_SPAWN_ENVIRONMENT_PROFILE_REVISION_BYTES + 1),
        )
        .is_err());

        assert!(!NodeRequest::SpawnSpec { spec: spec.clone() }
            .requires_child_environment_profile_capability());
        let mut explicit_environment = spec.clone();
        explicit_environment.overrides.environment_profile_id = SpawnOverride::Set {
            value: SpawnEnvironmentProfileId::new("local-default").unwrap(),
        };
        assert!(NodeRequest::SpawnSpec {
            spec: explicit_environment,
        }
        .requires_child_environment_profile_capability());
        let mut inherited_environment = spec.clone();
        inherited_environment.overrides.environment_profile_id = SpawnOverride::Inherit;
        assert!(!NodeRequest::SpawnSpec {
            spec: inherited_environment,
        }
        .requires_child_environment_profile_capability());

        let mut clear_required = spec.clone();
        clear_required.overrides.provider = SpawnOverride::Clear;
        assert!(matches!(
            clear_required.resolve(&defaults),
            Err(SpawnSpecResolveError::RequiredFieldCleared { field: "provider" }),
        ));

        let mut stale_revision = spec.clone();
        stale_revision.expected_profile_revision =
            SpawnProfileRevision::new("review-default.r2").unwrap();
        assert!(matches!(
            stale_revision.resolve(&defaults),
            Err(SpawnSpecResolveError::ProfileRevisionMismatch {
                expected,
                loaded,
            }) if expected.as_str() == "review-default.r2"
                && loaded.as_str() == "review-default.r3",
        ));

        let missing_revision_json = r#"{"target":{"node_id":"node-a","workspace_id":"primary"},"profile_id":"review-default","deadline_ms":1,"idempotency_key":"request-0002"}"#;
        assert!(serde_json::from_str::<SpawnSpec>(missing_revision_json).is_err());
        let minimal_json = r#"{"target":{"node_id":"node-a","workspace_id":"primary"},"profile_id":"review-default","expected_profile_revision":"review-default.r3","deadline_ms":1,"idempotency_key":"request-0002"}"#;
        let minimal = serde_json::from_str::<SpawnSpec>(minimal_json).unwrap();
        assert_eq!(minimal.overrides, SpawnOverrides::default());
        assert!(minimal.required_capabilities.is_empty());
        let mut unknown_field = serde_json::to_value(&minimal).unwrap();
        unknown_field["profile_revision"] = serde_json::json!("review-default.r3");
        assert!(serde_json::from_value::<SpawnSpec>(unknown_field).is_err());
        assert!(SpawnDeadlineMs::new(MAX_SPAWN_DEADLINE_MS + 1).is_err());
        assert!(SpawnProfileId::new("unsafe/profile").is_err());
        assert!(serde_json::from_str::<SpawnOverride<AgentId>>(
            r#"{"kind":"set","value":"claude","typo":true}"#,
        )
        .is_err());
    }

    #[test]
    fn session_bundle_materialization_contract_is_bounded_exact_and_dual_gated() {
        assert_eq!(NODE_PROTOCOL_VERSION, 11);
        assert_eq!(NODE_STATE_SCHEMA_V7, 7);
        assert_eq!(NODE_STATE_SCHEMA_V8, 8);
        assert_eq!(
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
            "session-bundle-materialization-v1",
        );
        let capability =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .contains(&capability));
        let mut support = portable_node_support();
        support.capabilities = vec![capability.clone()];
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = support
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(selected.capabilities, vec![capability]);
        let bound = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert!(bound
            .windows(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY.len())
            .any(|window| {
                window == NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY.as_bytes()
            }));
        assert!(SpawnBundleRevision::new(
            "r".repeat(MAX_SPAWN_BUNDLE_REVISION_BYTES),
        )
        .is_ok());
        assert!(SpawnBundleRevision::new(
            "r".repeat(MAX_SPAWN_BUNDLE_REVISION_BYTES + 1),
        )
        .is_err());

        let digest = format!("sha256:{}", "a".repeat(64));
        let receipt = ResolvedBundleReceipt {
            id: SpawnBundleId::new("review-bundle").unwrap(),
            revision: SpawnBundleRevision::new("review-bundle.r1").unwrap(),
            digest: SpawnBundleDigest::new(&digest).unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&receipt).unwrap(),
            format!(
                r#"{{"id":"review-bundle","revision":"review-bundle.r1","digest":"{digest}"}}"#,
            ),
        );
        for invalid in [
            format!("sha256:{}", "a".repeat(63)),
            format!("sha256:{}", "A".repeat(64)),
            "sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
        ] {
            assert!(SpawnBundleDigest::new(invalid).is_err());
        }

        let minimal_json = r#"{"target":{"node_id":"node-a","workspace_id":"primary"},"profile_id":"review-default","expected_profile_revision":"review-default.r3","deadline_ms":1,"idempotency_key":"request-bundle"}"#;
        let inherited = serde_json::from_str::<SpawnSpec>(minimal_json).unwrap();
        assert!(NodeRequest::SpawnSpec {
            spec: inherited.clone(),
        }
        .requires_session_bundle_materialization_capability());

        let mut explicit = inherited.clone();
        explicit.overrides.bundle_id = SpawnOverride::Set {
            value: SpawnBundleId::new("review-bundle").unwrap(),
        };
        assert!(NodeRequest::SpawnSpec { spec: explicit }
            .requires_session_bundle_materialization_capability());

        let mut cleared = inherited;
        cleared.overrides.bundle_id = SpawnOverride::Clear;
        assert!(!NodeRequest::SpawnSpec { spec: cleared }
            .requires_session_bundle_materialization_capability());
    }

    #[test]
    fn provider_ids_preserve_legacy_json_and_accept_open_values() {
        for (provider, expected) in [
            (agent("claude"), r#""claude""#),
            (agent("codex"), r#""codex""#),
            (agent("kimi"), r#""kimi""#),
        ] {
            assert!(provider_id_is_legacy(&provider));
            assert_eq!(serde_json::to_string(&provider).unwrap(), expected);
            assert_eq!(serde_json::from_str::<AgentId>(expected).unwrap(), provider);
        }

        let open = agent("qwen-3");
        assert!(!provider_id_is_legacy(&open));
        assert_eq!(serde_json::to_string(&open).unwrap(), r#""qwen-3""#);
    }

    #[test]
    fn provider_runtime_wire_is_exact_bounded_and_consistent() {
        let statuses = ProviderRuntimeStatuses::new([
            ProviderRuntimeStatus::raw_passthrough(
                agent("claude"),
                Some(ProviderRuntimeVersion::new("2.1.220").unwrap()),
            ),
            ProviderRuntimeStatus::verified_semantic(
                agent("codex"),
                ProviderRuntimeVersion::new("0.147.0").unwrap(),
                ProviderRuntimeContractId::new("codex.windows-x86_64.0.147.0").unwrap(),
            ),
            ProviderRuntimeStatus::unavailable(agent("kimi")),
        ])
        .unwrap();
        let encoded = serde_json::to_string(&statuses).unwrap();
        assert_eq!(
            encoded,
            r#"[{"provider":"claude","mode":"raw-passthrough","version":"2.1.220"},{"provider":"codex","mode":"verified-semantic","version":"0.147.0","contract_id":"codex.windows-x86_64.0.147.0"},{"provider":"kimi","mode":"unavailable"}]"#,
        );
        assert_eq!(
            serde_json::from_str::<ProviderRuntimeStatuses>(&encoded).unwrap(),
            statuses,
        );

        let duplicate = r#"[{"provider":"codex","mode":"unavailable"},{"provider":"codex","mode":"unavailable"}]"#;
        assert!(serde_json::from_str::<ProviderRuntimeStatuses>(duplicate).is_err());
        let too_many = (0..=MAX_PROVIDER_RUNTIME_STATUSES)
            .map(|index| ProviderRuntimeStatus::unavailable(agent(&format!("provider-{index}"))))
            .collect::<Vec<_>>();
        assert!(matches!(
            ProviderRuntimeStatuses::new(too_many),
            Err(ProviderRuntimeStatusError::TooMany {
                max: MAX_PROVIDER_RUNTIME_STATUSES,
            }),
        ));
        let inconsistent = r#"[{"provider":"codex","mode":"verified-semantic","version":"0.147.0"}]"#;
        assert!(serde_json::from_str::<ProviderRuntimeStatuses>(inconsistent).is_err());
        for arbitrary in ["secret-token", "raw.stdout", "1.2", "01.2.3", "1.2.3.4"] {
            assert!(ProviderRuntimeVersion::new(arbitrary).is_err(), "accepted {arbitrary}");
        }
        let overflow = format!(
            r#"[{{"provider":"codex","mode":"raw-passthrough","version":"{}"}}]"#,
            "1".repeat(MAX_PROVIDER_RUNTIME_VERSION_BYTES + 1),
        );
        assert!(serde_json::from_str::<ProviderRuntimeStatuses>(&overflow).is_err());
    }

    #[test]
    fn legacy_node_snapshot_defaults_runtime_status_to_unreported() {
        let legacy = r#"{"node_id":"legacy-node","enabled_providers":["codex"],"workspaces":[]}"#;
        let snapshot = serde_json::from_str::<NodeSnapshot>(legacy).unwrap();
        assert!(snapshot.provider_runtime_statuses.is_empty());
        let reencoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!reencoded.contains("provider_runtime"));
    }

    #[test]
    fn compatibility_negotiation_keeps_the_active_wire_and_selects_highest_state_schema() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(8, 10).unwrap(),
            capabilities: vec![
                CapabilityId::new("session.spawn").unwrap(),
                CapabilityId::new("unknown.future").unwrap(),
            ],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::new(4, 6).unwrap(),
            }),
        };
        let hello = ClientHello::negotiating(
            ClientRole::Operator,
            [1; NODE_AUTH_NONCE_BYTES],
            offer.clone(),
        );
        assert_eq!(hello.compatibility, Some(offer.clone()));

        let negotiated = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(negotiated.protocol_version, NODE_PROTOCOL_VERSION);
        assert_eq!(negotiated.state_schema_version, Some(5));
        assert_eq!(
            negotiated.capabilities,
            vec![CapabilityId::new("session.spawn").unwrap()],
        );
        assert!(negotiated.provider_contracts.is_empty());
        assert!(negotiated.provider_adapter_contracts.is_empty());
    }

    #[test]
    fn compatibility_negotiation_rejects_an_active_wire_outside_either_range() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(10, 11).unwrap(),
            capabilities: Vec::new(),
            state_schema: None,
        };
        let error = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap_err();
        assert!(matches!(
            error,
            ProtocolNegotiationError::ActiveVersionUnsupported {
                active: NODE_PROTOCOL_VERSION,
                ..
            },
        ));
    }

    #[test]
    fn compatibility_auth_binding_has_an_exact_bounded_encoding() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(8, 10).unwrap(),
            capabilities: vec![CapabilityId::new("session.spawn").unwrap()],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::new(4, 6).unwrap(),
            }),
        };
        let selected = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        let encoded = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":10},"capabilities":["session.spawn"],"state_schema":{"versions":{"minimum":4,"maximum":6}}},"selected":{"protocol_version":9,"capabilities":["session.spawn"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","state_schema_version":5,"provider_contracts":[]}}"#,
        );
    }

    #[test]
    fn provider_contract_manifest_is_capability_gated_and_auth_bound_exactly() {
        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities.push(manifest_capability.clone());
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![manifest_capability.clone()],
            state_schema: None,
        };
        let selected = support
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(selected.capabilities, vec![manifest_capability]);
        assert_eq!(selected.provider_contracts, support.provider_contracts);
        assert_eq!(
            selected.provider_adapter_contracts,
            support.provider_adapter_contracts,
        );
        let encoded = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":9,"maximum":9},"capabilities":["provider-contract-manifest-v1"]},"selected":{"protocol_version":9,"capabilities":["provider-contract-manifest-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[{"provider":"codex","revision":"codex.2026-08"}],"provider_adapter_contracts":[{"provider":"codex","family":"pty-semantic","adapter_id":"codex-cli","revision":"pty-semantic-v1"}]}}"#,
        );
    }

    #[test]
    fn open_provider_capability_and_manifest_are_auth_bound_exactly() {
        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        let open_capability = CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities = vec![manifest_capability.clone(), open_capability.clone()];
        support.provider_contracts = vec![ProviderContractSupport {
            provider: agent("qwen"),
            revision: ProviderContractRevision::new("qwen.2026-08").unwrap(),
        }];
        support.provider_adapter_contracts.clear();
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![manifest_capability, open_capability],
            state_schema: None,
        };
        let selected = support
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        let encoded = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":9,"maximum":9},"capabilities":["provider-contract-manifest-v1","provider-id.open-v1"]},"selected":{"protocol_version":9,"capabilities":["provider-contract-manifest-v1","provider-id.open-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[{"provider":"qwen","revision":"qwen.2026-08"}]}}"#,
        );
    }

    #[test]
    fn n_minus_one_selected_json_round_trips_without_manifest_fields() {
        let json = r#"{"protocol_version":8,"capabilities":["compatibility.metadata"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","state_schema_version":1,"provider_contracts":[]}"#;
        let selected: NegotiatedNodeCompatibility = serde_json::from_str(json).unwrap();
        assert!(selected.provider_contracts.is_empty());
        assert!(selected.provider_adapter_contracts.is_empty());
        assert_eq!(serde_json::to_string(&selected).unwrap(), json);
    }

    #[test]
    fn provider_contract_manifest_rejects_bounds_duplicates_and_unlinked_entries() {
        let provider = ProviderContractSupport {
            provider: agent("codex"),
            revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
        };
        let adapter = ProviderAdapterContractSupport {
            provider: agent("codex"),
            family: AdapterFamily::PtySemantic,
            adapter_id: AdapterId::new("codex-cli").unwrap(),
            revision: AdapterContractRevision::new("pty-semantic-v1").unwrap(),
        };
        assert!(matches!(
            validate_provider_contract_manifest(
                &vec![provider.clone(); MAX_PROVIDER_CONTRACTS + 1],
                &[],
            ),
            Err(ProviderContractManifestError::TooManyProviders { .. }),
        ));
        assert!(matches!(
            validate_provider_contract_manifest(
                &[provider.clone()],
                &vec![adapter.clone(); MAX_PROVIDER_ADAPTER_CONTRACTS + 1],
            ),
            Err(ProviderContractManifestError::TooManyAdapterContracts { .. }),
        ));
        assert!(matches!(
            validate_provider_contract_manifest(
                &[provider.clone()],
                &vec![adapter.clone(); MAX_PROVIDER_ADAPTER_CONTRACTS],
            ),
            Err(ProviderContractManifestError::DuplicateProviderFamily { .. }),
        ));
        assert!(matches!(
            validate_provider_contract_manifest(&[provider.clone(), provider.clone()], &[]),
            Err(ProviderContractManifestError::DuplicateProvider { provider })
                if provider == agent("codex"),
        ));
        let mut unlinked = adapter.clone();
        unlinked.provider = agent("claude");
        assert!(matches!(
            validate_provider_contract_manifest(&[provider.clone()], &[unlinked]),
            Err(ProviderContractManifestError::UnlinkedAdapterProvider { provider })
                if provider == agent("claude"),
        ));
        let mut duplicate_family = adapter.clone();
        duplicate_family.adapter_id = AdapterId::new("codex-cli-next").unwrap();
        assert!(matches!(
            validate_provider_contract_manifest(
                &[provider.clone()],
                &[adapter.clone(), duplicate_family],
            ),
            Err(ProviderContractManifestError::DuplicateProviderFamily {
                provider,
                family: AdapterFamily::PtySemantic,
            }) if provider == agent("codex"),
        ));
        let mut shared_adapter_id = adapter.clone();
        shared_adapter_id.family = AdapterFamily::History;
        assert!(validate_provider_contract_manifest(
            &[provider],
            &[adapter, shared_adapter_id],
        )
        .is_ok());
    }

    #[test]
    fn provider_contract_manifest_negotiates_all_current_provider_family_tuples() {
        let providers = [
            (agent("claude"), "claude-code", "claude.2026-08"),
            (agent("codex"), "codex-cli", "codex.2026-08"),
            (agent("kimi"), "kimi-cli", "kimi.2026-08"),
        ];
        let families = [
            AdapterFamily::PtySemantic,
            AdapterFamily::Pipe,
            AdapterFamily::OneShot,
            AdapterFamily::Acp,
            AdapterFamily::Hook,
            AdapterFamily::ManagedHook,
            AdapterFamily::History,
            AdapterFamily::Resume,
            AdapterFamily::SessionOptions,
            AdapterFamily::CapabilityProbe,
        ];
        let provider_contracts = providers
            .iter()
            .map(|(provider, _, revision)| ProviderContractSupport {
                provider: provider.clone(),
                revision: ProviderContractRevision::new(*revision).unwrap(),
            })
            .collect::<Vec<_>>();
        let provider_adapter_contracts = providers
            .iter()
            .flat_map(|(provider, adapter_id, _)| {
                families
                    .iter()
                    .map(move |family| ProviderAdapterContractSupport {
                        provider: provider.clone(),
                        family: *family,
                        adapter_id: AdapterId::new(*adapter_id).unwrap(),
                        revision: AdapterContractRevision::new("adapter-contract-v1").unwrap(),
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(provider_contracts.len(), 3);
        assert_eq!(provider_adapter_contracts.len(), 30);
        assert!(provider_adapter_contracts.len() <= MAX_PROVIDER_ADAPTER_CONTRACTS);

        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities.push(manifest_capability.clone());
        support.provider_contracts = provider_contracts;
        support.provider_adapter_contracts = provider_adapter_contracts;
        let selected = support
            .negotiate(
                NODE_PROTOCOL_VERSION,
                &ClientCompatibilityOffer {
                    protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
                    capabilities: vec![manifest_capability],
                    state_schema: None,
                },
            )
            .unwrap();
        assert_eq!(selected.provider_contracts.len(), 3);
        assert_eq!(selected.provider_adapter_contracts.len(), 30);
    }

    #[test]
    fn sixteen_provider_manifest_headroom_fits_the_authenticated_handshake() {
        assert_eq!(MAX_PROVIDER_IDENTITIES, 16);
        let providers = (0..MAX_PROVIDER_IDENTITIES)
            .map(|index| agent(&format!("provider-{index}")))
            .collect::<Vec<_>>();
        let mut support = portable_node_support();
        support.capabilities.extend([
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
        ]);
        support.provider_contracts = providers
            .iter()
            .map(|provider| ProviderContractSupport {
                provider: provider.clone(),
                revision: ProviderContractRevision::new(format!(
                    "{}.2026-08",
                    provider.as_str(),
                ))
                .unwrap(),
            })
            .collect();
        support.provider_adapter_contracts = providers
            .iter()
            .map(|provider| ProviderAdapterContractSupport {
                provider: provider.clone(),
                family: AdapterFamily::PtySemantic,
                adapter_id: AdapterId::new(format!("{}-cli", provider.as_str())).unwrap(),
                revision: AdapterContractRevision::new("pty-semantic-v1").unwrap(),
            })
            .collect();

        validate_node_negotiated_handshake_capacity(&support, NODE_PROTOCOL_VERSION).unwrap();
    }

    #[test]
    fn adapter_contract_revision_is_bounded_printable_ascii() {
        assert!(AdapterContractRevision::new("history-jsonl-v1").is_ok());
        assert!(AdapterContractRevision::new("").is_err());
        assert!(AdapterContractRevision::new("revision with spaces").is_err());
        assert!(AdapterContractRevision::new(
            "x".repeat(MAX_ADAPTER_CONTRACT_REVISION_BYTES + 1),
        )
        .is_err());
    }

    #[test]
    fn protocol_ranges_reject_invalid_and_disjoint_inputs() {
        assert!(matches!(
            ProtocolRange::new(0, 8),
            Err(ProtocolNegotiationError::InvalidRange { minimum: 0, maximum: 8 }),
        ));
        assert!(matches!(
            ProtocolRange::new(9, 8),
            Err(ProtocolNegotiationError::InvalidRange { minimum: 9, maximum: 8 }),
        ));
        let error = ProtocolRange::new(7, 8)
            .unwrap()
            .highest_common(ProtocolRange::new(9, 10).unwrap())
            .unwrap_err();
        assert!(matches!(error, ProtocolNegotiationError::Disjoint { .. }));
        assert!(serde_json::from_str::<ProtocolRange>(
            r#"{"minimum":10,"maximum":9}"#,
        )
        .is_err());
        assert!(ProviderContractRevision::new(
            "gate4agent-inline/codex-cli-0.144/v1",
        )
        .is_ok());
        assert!(ProviderContractRevision::new(
            "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42",
        )
        .is_ok());
    }

    #[test]
    fn foreign_path_metadata_round_trips_without_normalization() {
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct ForeignPath {
            raw: String,
            semantics: PathSemantics,
        }

        let foreign = ForeignPath {
            raw: r"C:\Users\operator\repo\src\lib.rs".to_owned(),
            semantics: portable_node_support().path_semantics,
        };
        let json = serde_json::to_string(&foreign).unwrap();
        assert!(json.contains(r#""raw":"C:\\Users\\operator\\repo\\src\\lib.rs""#));
        assert_eq!(serde_json::from_str::<ForeignPath>(&json).unwrap(), foreign);
    }

    #[test]
    fn opaque_host_path_utf8_preserves_legacy_json_string_shape() {
        let path = host_path(r"C:\Users\operator\repo");
        assert_eq!(path.byte_len(), 22);
        assert_eq!(path.as_utf8(), Some(r"C:\Users\operator\repo"));
        assert_eq!(path.as_unix_bytes(), None);
        assert_eq!(path.display_text(), r"C:\Users\operator\repo");

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, r#""C:\\Users\\operator\\repo""#);
        assert_eq!(serde_json::from_str::<OpaqueHostPath>(&json).unwrap(), path);
    }

    #[test]
    fn opaque_host_path_unix_bytes_has_strict_bounded_tagged_wire_shape() {
        let raw = vec![b'/', b'r', b'e', b'p', b'o', b'/', 0xff];
        let path = OpaqueHostPath::unix_bytes(raw.clone()).unwrap();
        assert_eq!(path.byte_len(), raw.len());
        assert_eq!(path.as_utf8(), None);
        assert_eq!(path.as_unix_bytes(), Some(raw.as_slice()));

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"unix-bytes","bytes":[47,114,101,112,111,47,255]}"#,
        );
        assert_eq!(serde_json::from_str::<OpaqueHostPath>(&json).unwrap(), path);

        for invalid in [
            r#"{"kind":"future","bytes":[47]}"#,
            r#"{"kind":"unix-bytes","bytes":[47],"extra":true}"#,
            r#"{"kind":"unix-bytes"}"#,
            r#"{"bytes":[47]}"#,
            r#"{"kind":"unix-bytes","bytes":[]}"#,
            r#"{"kind":"unix-bytes","bytes":[47,0]}"#,
        ] {
            assert!(serde_json::from_str::<OpaqueHostPath>(invalid).is_err(), "{invalid}");
        }
        assert!(OpaqueHostPath::utf8(String::new()).is_err());
        assert!(OpaqueHostPath::utf8("a\0b".to_owned()).is_err());
        assert!(OpaqueHostPath::utf8("x".repeat(MAX_WORKSPACE_ROOT_BYTES + 1)).is_err());
        assert!(OpaqueHostPath::unix_bytes(vec![b'x'; MAX_WORKSPACE_ROOT_BYTES + 1]).is_err());
    }

    #[test]
    fn repository_path_utf8_preserves_legacy_json_string_shape_and_typed_components() {
        let path = repository_path(r"src\literal-name/lib.rs");
        assert_eq!(path.byte_len(), 23);
        assert_eq!(path.as_utf8(), Some(r"src\literal-name/lib.rs"));
        assert_eq!(path.as_unix_bytes(), None);
        assert_eq!(path.display_text(), r"src\literal-name/lib.rs");
        assert_eq!(path.component_count(), 2);
        assert_eq!(path.depth(), 1);
        assert_eq!(path.file_name_bytes(), b"lib.rs");
        assert_eq!(path.file_name_display_text(), "lib.rs");
        assert!(path.is_descendant_of(&repository_path(r"src\literal-name")));
        assert!(!path.is_descendant_of(&repository_path("src")));

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, r#""src\\literal-name/lib.rs""#);
        assert_eq!(serde_json::from_str::<RepositoryPath>(&json).unwrap(), path);
    }

    #[test]
    fn repository_path_unix_bytes_has_strict_bounded_tagged_wire_shape() {
        let raw = vec![b's', b'r', b'c', b'/', 0xff, b'/', b'f'];
        let path = RepositoryPath::unix_bytes(raw.clone()).unwrap();
        assert_eq!(path.byte_len(), raw.len());
        assert_eq!(path.as_bytes(), raw);
        assert_eq!(path.as_utf8(), None);
        assert_eq!(path.as_unix_bytes(), Some(raw.as_slice()));
        assert_eq!(path.component_count(), 3);
        assert_eq!(path.depth(), 2);
        assert_eq!(path.file_name_bytes(), b"f");
        assert!(path.is_descendant_of(
            &RepositoryPath::unix_bytes(vec![b's', b'r', b'c', b'/', 0xff]).unwrap(),
        ));

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"unix-bytes","bytes":[115,114,99,47,255,47,102]}"#,
        );
        assert_eq!(serde_json::from_str::<RepositoryPath>(&json).unwrap(), path);
    }

    #[test]
    fn repository_path_physical_identity_ignores_wire_representation() {
        use std::collections::{BTreeSet, HashSet};

        let utf8 = repository_path("src/main.rs");
        let tagged = RepositoryPath::unix_bytes(b"src/main.rs".to_vec()).unwrap();
        assert_eq!(utf8, tagged);
        assert_eq!(utf8.cmp(&tagged), Ordering::Equal);
        assert_eq!(utf8.as_unix_bytes(), None);
        assert_eq!(tagged.as_unix_bytes(), Some(b"src/main.rs".as_slice()));
        assert_eq!(serde_json::to_string(&utf8).unwrap(), r#""src/main.rs""#);
        assert_eq!(
            serde_json::to_string(&tagged).unwrap(),
            r#"{"kind":"unix-bytes","bytes":[115,114,99,47,109,97,105,110,46,114,115]}"#,
        );

        let mut ordered = BTreeSet::new();
        ordered.insert(utf8.clone());
        ordered.insert(tagged.clone());
        assert_eq!(ordered.len(), 1);

        let mut hashed = HashSet::new();
        hashed.insert(utf8);
        hashed.insert(tagged);
        assert_eq!(hashed.len(), 1);

        let backslash = repository_path(r"src\main.rs");
        let slash = repository_path("src/main.rs");
        assert_ne!(backslash, slash);
        assert_ne!(backslash.cmp(&slash), Ordering::Equal);
    }

    #[test]
    fn repository_path_rejects_non_canonical_or_unbounded_components() {
        for invalid in [
            "",
            "/",
            "/src",
            "src/",
            "src//lib.rs",
            ".",
            "..",
            "./src",
            "src/.",
            "../src",
            "src/..",
            "src\0lib.rs",
        ] {
            assert!(RepositoryPath::utf8(invalid.to_owned()).is_err(), "{invalid:?}");
        }
        assert!(RepositoryPath::utf8("x".repeat(MAX_REPOSITORY_PATH_BYTES + 1)).is_err());
        assert!(RepositoryPath::unix_bytes(vec![b'x'; MAX_REPOSITORY_PATH_BYTES + 1]).is_err());

        for invalid in [
            r#"{"kind":"future","bytes":[115]}"#,
            r#"{"kind":"unix-bytes","bytes":[115],"extra":true}"#,
            r#"{"kind":"unix-bytes"}"#,
            r#"{"bytes":[115]}"#,
            r#"{"kind":"unix-bytes","bytes":[]}"#,
            r#"{"kind":"unix-bytes","bytes":[47,115]}"#,
            r#"{"kind":"unix-bytes","bytes":[115,47,46,46]}"#,
        ] {
            assert!(serde_json::from_str::<RepositoryPath>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn repository_path_fields_preserve_legacy_utf8_wire_bytes_and_default_rename_source() {
        let entry = WorkspaceEntry {
            relative_path: repository_path("src/lib.rs"),
            kind: WorkspaceEntryKind::File,
        };
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"relative_path":"src/lib.rs","kind":"file"}"#,
        );

        let legacy_status =
            r#"{"index_status":"R","worktree_status":" ","path":"src/new.rs"}"#;
        let status = serde_json::from_str::<GitStatusEntry>(legacy_status).unwrap();
        assert_eq!(status.path, repository_path("src/new.rs"));
        assert_eq!(status.previous_path, None);
        assert_eq!(serde_json::to_string(&status).unwrap(), legacy_status);

        let renamed = GitStatusEntry {
            previous_path: Some(repository_path("src/old.rs")),
            ..status
        };
        assert_eq!(
            serde_json::to_string(&renamed).unwrap(),
            r#"{"index_status":"R","worktree_status":" ","path":"src/new.rs","previous_path":"src/old.rs"}"#,
        );
    }

    #[tokio::test]
    async fn json_frame_round_trips_a_client_hello_without_the_access_token() {
        let expected = ClientFrame::Hello(ClientHello::new(ClientRole::Operator, [7; NODE_AUTH_NONCE_BYTES]));
        let mut wire = Vec::new();
        write_json_frame(&mut wire, &expected).await.unwrap();

        assert!(!String::from_utf8_lossy(&wire).contains("local-token"));

        let actual: ClientFrame = read_json_frame(&mut wire.as_slice()).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn reader_rejects_zero_length_before_allocating() {
        let bytes = 0_u32.to_le_bytes();
        let mut wire = bytes.as_slice();
        let error = read_json_frame::<_, ClientFrame>(&mut wire).await.unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength { length: 0, max: MAX_NODE_FRAME_BYTES }
        ));
    }

    #[tokio::test]
    async fn hello_reader_rejects_an_oversized_frame_before_allocating() {
        let declared = (MAX_NODE_HELLO_FRAME_BYTES + 1) as u32;
        let bytes = declared.to_le_bytes();
        let mut wire = bytes.as_slice();
        let error = read_json_frame_limited::<_, ClientFrame>(
            &mut wire,
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength {
                length,
                max: MAX_NODE_HELLO_FRAME_BYTES
            } if length == MAX_NODE_HELLO_FRAME_BYTES + 1
        ));
    }

    #[tokio::test]
    async fn client_writer_enforces_the_smaller_request_limit() {
        let oversized = ClientFrame::Request(RequestEnvelope {
            request_id: 1,
            request: NodeRequest::Input {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(1),
                        generation: SessionGeneration(1),
                    },
                },
                text: "x".repeat(MAX_NODE_CLIENT_FRAME_BYTES),
            },
        });
        let mut wire = Vec::new();
        let error = write_json_frame_limited(
            &mut wire,
            &oversized,
            MAX_NODE_CLIENT_FRAME_BYTES,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength {
                length,
                max: MAX_NODE_CLIENT_FRAME_BYTES
            } if length > MAX_NODE_CLIENT_FRAME_BYTES
        ));
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn maximum_node_text_fits_the_client_frame_under_worst_case_json_escaping() {
        let frame = ClientFrame::Request(RequestEnvelope {
            request_id: 2,
            request: NodeRequest::Paste {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(1),
                        generation: SessionGeneration(1),
                    },
                },
                text: "\0".repeat(MAX_NODE_TEXT_BYTES),
            },
        });
        let mut wire = Vec::new();
        write_json_frame_limited(&mut wire, &frame, MAX_NODE_CLIENT_FRAME_BYTES)
            .await
            .unwrap();
        assert!(wire.len() <= MAX_NODE_CLIENT_FRAME_BYTES + std::mem::size_of::<u32>());
    }

    #[tokio::test]
    async fn body_timeout_starts_after_the_bounded_length_prefix() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_u32_le(32).await.unwrap();
        writer.write_all(b"{").await.unwrap();
        let error = read_json_frame_limited_body_timeout::<_, ClientFrame>(
            &mut reader,
            MAX_NODE_CLIENT_FRAME_BYTES,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FrameError::BodyTimedOut { length: 32 }));
    }

    #[tokio::test]
    async fn partial_length_prefix_cannot_pin_a_connection_slot() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[32]).await.unwrap();
        let error = read_json_frame_limited_body_timeout::<_, ClientFrame>(
            &mut reader,
            MAX_NODE_CLIENT_FRAME_BYTES,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FrameError::PrefixTimedOut));
    }

    #[test]
    fn resume_wire_does_not_accept_a_replacement_working_directory() {
        let frame = ClientFrame::Request(RequestEnvelope {
            request_id: 9,
            request: NodeRequest::Resume {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(3),
                        generation: SessionGeneration(2),
                    },
                },
                terminal_size: TerminalSize { rows: 24, columns: 80 },
                initial_prompt: Some("continue".to_owned()),
            },
        });
        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains("working_directory"));

        let mut malicious = serde_json::to_value(match frame {
            ClientFrame::Request(envelope) => envelope.request,
            _ => unreachable!("constructed request frame"),
        })
        .unwrap();
        malicious
            .as_object_mut()
            .unwrap()
            .insert(
                "working_directory".to_owned(),
                serde_json::Value::String(r"C:\attacker-selected-root".to_owned()),
            );
        let error = serde_json::from_value::<NodeRequest>(malicious).unwrap_err();
        assert!(error.to_string().contains("unknown field `working_directory`"));
    }

    #[test]
    fn history_context_pack_wire_is_bounded_path_free_and_auth_bound() {
        assert_eq!(NODE_PROTOCOL_VERSION, 11);
        assert_eq!(NODE_HISTORY_CONTEXT_PACK_CAPABILITY, "history-context-pack-v1");
        let capability = CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap();
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .contains(&capability));
        let mut support = portable_node_support();
        support.capabilities = vec![capability.clone()];
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = support.negotiate(NODE_PROTOCOL_VERSION, &offer).unwrap();
        assert_eq!(selected.capabilities, vec![capability]);
        let binding = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert!(binding
            .windows(NODE_HISTORY_CONTEXT_PACK_CAPABILITY.len())
            .any(|window| window == NODE_HISTORY_CONTEXT_PACK_CAPABILITY.as_bytes()));

        let request = NodeRequest::DiscoverHistory {
            session: session_address("primary", 3),
            limit: HISTORY_DISCOVERY_LIMIT_MAX,
        };
        assert_eq!(
            request.required_capability(),
            Some(NODE_HISTORY_CONTEXT_PACK_CAPABILITY),
        );
        assert!(request.history_context_pack_contract_is_valid());
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("working_directory"));
        assert!(!json.contains("path"));
        assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);

        let mut invalid = serde_json::to_value(&request).unwrap();
        invalid["limit"] = serde_json::Value::from(0);
        assert!(serde_json::from_value::<NodeRequest>(invalid).is_err());
        assert!(!NodeRequest::DiscoverHistory {
            session: session_address("primary", 3),
            limit: 0,
        }
        .history_context_pack_contract_is_valid());
        assert!(!NodeRequest::LoadHistory {
            session: session_address("primary", 3),
            candidate_id: String::new(),
        }
        .history_context_pack_contract_is_valid());
        let mut invalid = serde_json::to_value(&request).unwrap();
        invalid["limit"] = serde_json::Value::from(u64::from(HISTORY_DISCOVERY_LIMIT_MAX) + 1);
        assert!(serde_json::from_value::<NodeRequest>(invalid).is_err());
        let mut invalid = serde_json::to_value(&request).unwrap();
        invalid["working_directory"] =
            serde_json::Value::String(r"C:\attacker-selected-root".to_owned());
        assert!(serde_json::from_value::<NodeRequest>(invalid).is_err());

        let response = NodeResponse::HistoryDiscovered {
            session: session_address("primary", 3),
            candidates: vec![HistoryCandidateSummary {
                id: "candidate-1".to_owned(),
                session_id_hint: "session-1".to_owned(),
                modified_at_unix_ms: Some(1),
            }],
        };
        assert!(response.requires_history_context_pack_capability());
        let response_json = serde_json::to_string(&response).unwrap();
        assert!(!response_json.contains("messages"));
        assert!(!response_json.contains("working_directory"));
        assert!(!response_json.contains("path"));
        assert_eq!(serde_json::from_str::<NodeResponse>(&response_json).unwrap(), response);
    }

    #[test]
    fn native_session_catalog_wire_is_bounded_metadata_only() {
        assert_eq!(NODE_NATIVE_SESSION_CATALOG_CAPABILITY, "native-session-catalog-v2");
        let route = NativeSessionCatalogRoute::workspace(
            WorkspaceId::new("primary").unwrap(),
            agent("codex"),
        );
        let request = NodeRequest::CatalogNativeSessions {
            route: route.clone(),
            limit: NATIVE_SESSION_CATALOG_LIMIT_MAX,
        };
        assert_eq!(
            request.required_capability(),
            Some(NODE_NATIVE_SESSION_CATALOG_CAPABILITY)
        );
        assert!(request.native_session_catalog_contract_is_valid());
        let mut invalid = serde_json::to_value(&request).unwrap();
        invalid["limit"] = serde_json::Value::from(65);
        assert!(serde_json::from_value::<NodeRequest>(invalid).is_err());

        let response = NodeResponse::NativeSessionsCataloged {
            route: route.clone(),
            entries: vec![NativeSessionCatalogEntry {
                selection_id: "hist_selection_1".to_owned(),
                title: Some("Review".to_owned()),
                modified_at_unix_ms: Some(9),
                model: Some("model-1".to_owned()),
                message_count: 4,
                completed_turn_count: Some(2),
                external_group: None,
                record_id: Some(SessionRecordId::new("record-1").unwrap()),
            }],
            summary: Some(NativeSessionCatalogSummary {
                catalog_revision: 7,
                recent_cutoff_unix_ms: 8,
                recent_total_count: 1,
                older_total_count: 2,
                recent_next_after_selection_id: None,
                recent_has_more: false,
            }),
        };
        assert!(response.requires_native_session_catalog_capability());
        assert!(response.native_session_catalog_contract_is_valid());
        let json = serde_json::to_string(&response).unwrap();
        for forbidden in ["session_id", "stable-session", "cwd", "candidate", "path", "messages", "tokens", "documents", "raw"] {
            assert!(!json.contains(forbidden));
        }
        assert_eq!(serde_json::from_str::<NodeResponse>(&json).unwrap(), response);
        assert!(serde_json::from_str::<NodeResponse>(
            r#"{"kind":"native-sessions-cataloged","workspace_id":"primary","provider":"codex","entries":[]}"#,
        )
        .is_err());
        let mut injected = serde_json::to_value(&response).unwrap();
        injected["entries"][0]["title"] = serde_json::Value::String("safe\nunsafe".to_owned());
        assert!(serde_json::from_value::<NodeResponse>(injected).is_err());

        let external = NodeResponse::NativeSessionsCataloged {
            route: NativeSessionCatalogRoute::unregistered(agent("codex")),
            entries: vec![NativeSessionCatalogEntry {
                selection_id: "external_selection_1".to_owned(),
                title: None,
                modified_at_unix_ms: Some(9),
                model: None,
                message_count: 0,
                completed_turn_count: None,
                external_group: Some(NativeSessionExternalGroup {
                    group_id: "external-0001".to_owned(),
                    kind: gate4agent_types::NativeSessionExternalGroupKind::Project,
                    display_name: "shared".to_owned(),
                }),
                record_id: None,
            }],
            summary: Some(NativeSessionCatalogSummary {
                catalog_revision: 9,
                recent_cutoff_unix_ms: 8,
                recent_total_count: 1,
                older_total_count: 0,
                recent_next_after_selection_id: None,
                recent_has_more: false,
            }),
        };
        assert!(external.native_session_catalog_contract_is_valid());
        let external_json = serde_json::to_string(&external).unwrap();
        assert!(!external_json.contains("project-"));
        for hostile in [r"C:\private", "/srv/private", "..", "nested/path"] {
            let mut injected = serde_json::to_value(&external).unwrap();
            injected["entries"][0]["external_group"]["display_name"] =
                serde_json::Value::String(hostile.to_owned());
            assert!(serde_json::from_value::<NodeResponse>(injected).is_err());
        }

        let page_request = NodeRequest::PageNativeSessions {
            route: route.clone(),
            window: NativeSessionCatalogWindow::Older,
            catalog_revision: 7,
            recent_cutoff_unix_ms: 8,
            after_selection_id: Some("hist_selection_1".to_owned()),
            limit: 1,
        };
        assert_eq!(
            page_request.required_capability(),
            Some(NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY),
        );
        let paged = NodeResponse::NativeSessionsPaged {
            route,
            page: NativeSessionCatalogPage {
                window: NativeSessionCatalogWindow::Older,
                revision: 7,
                entries: Vec::new(),
                next_after_selection_id: None,
                remaining_count: 0,
                has_more: false,
            },
        };
        assert!(paged.requires_native_session_catalog_paging_capability());
        assert!(paged.native_session_catalog_contract_is_valid());
    }

    #[test]
    fn native_session_preview_wire_is_bounded_and_record_projection_is_redacted() {
        assert_eq!(NODE_NATIVE_SESSION_PREVIEW_CAPABILITY, "native-session-preview-v2");
        let selection = NativeSessionSelection {
            route: NativeSessionCatalogRoute::workspace(
                WorkspaceId::new("primary").unwrap(),
                agent("claude"),
            ),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 8,
            selection_id: "hist_selection_1".to_owned(),
        };
        let request = NodeRequest::PreviewNativeSession {
            selection: selection.clone(),
            message_limit: NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX,
        };
        assert_eq!(
            request.required_capability(),
            Some(NODE_NATIVE_SESSION_PREVIEW_CAPABILITY)
        );
        assert!(request.native_session_preview_contract_is_valid());
        let mut invalid = serde_json::to_value(&request).unwrap();
        invalid["message_limit"] = serde_json::Value::from(25);
        assert!(serde_json::from_value::<NodeRequest>(invalid).is_err());

        let preview = NativeSessionPreview {
            title: Some("Review".to_owned()),
            modified_at_unix_ms: Some(9),
            model: Some("model-1".to_owned()),
            message_count: 8,
            message_count_exact: true,
            completed_turn_count: Some(4),
            total_tokens: None,
            truncated: true,
            messages: vec![gate4agent_types::NativeSessionPreviewMessage {
                role: gate4agent_types::HistoryMessageRole::Assistant,
                text: "visible answer".to_owned(),
            }],
        };
        let response = NodeResponse::NativeSessionPreviewed {
            selection,
            preview,
        };
        assert!(response.requires_native_session_preview_capability());
        let json = serde_json::to_string(&response).unwrap();
        for forbidden in ["native-secret-id", "cwd", "path", "tokens", "tool_result", "thinking"] {
            assert!(!json.contains(forbidden));
        }
        assert_eq!(serde_json::from_str::<NodeResponse>(&json).unwrap(), response);
    }

    #[test]
    fn context_receipts_are_nonempty_and_strictly_correlated() {
        assert!(SpawnContextDigest::new(format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(SpawnContextDigest::new(format!("sha256:{}", "A".repeat(64))).is_err());
        let context = context_receipt("context-1", session_address("primary", 7));
        let record = ManagedSessionRecord {
            record_id: SessionRecordId::new("session-1").unwrap(),
            display_name: "review".to_owned(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: host_path(r"C:\repo"),
            provider_session: None,
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: Some(context.id.clone()),
            context: Some(context.clone()),
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
            last_error: None,
        };
        assert!(record.context_binding_is_valid());
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(serde_json::from_value::<ManagedSessionRecord>(json.clone()).unwrap(), record);

        let mut invalid_task_revision = json.clone();
        invalid_task_revision["task_binding"] = serde_json::json!({
            "revision": 0,
            "task_id": "task-00112233445566778899aaff",
            "changed_at_unix_ms": 1,
        });
        assert!(serde_json::from_value::<ManagedSessionRecord>(invalid_task_revision).is_err());
        let mut invalid_task_timestamp = json.clone();
        invalid_task_timestamp["task_binding"] = serde_json::json!({
            "revision": 1,
            "task_id": "task-00112233445566778899aaff",
            "changed_at_unix_ms": 3,
        });
        assert!(serde_json::from_value::<ManagedSessionRecord>(invalid_task_timestamp).is_err());

        let mut metadata_without_id = json.clone();
        metadata_without_id["context_id"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<ManagedSessionRecord>(metadata_without_id).is_err());
        let mut id_without_metadata = json.clone();
        id_without_metadata["context"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<ManagedSessionRecord>(id_without_metadata).is_err());
        let mut mismatched = json.clone();
        mismatched["context_id"] = serde_json::Value::String("context-2".to_owned());
        assert!(serde_json::from_value::<ManagedSessionRecord>(mismatched).is_err());
        let mut missing_truncation = json.clone();
        missing_truncation["context"]["truncated"] = serde_json::Value::Bool(false);
        assert!(serde_json::from_value::<ManagedSessionRecord>(missing_truncation).is_err());
        let mut false_truncation = json.clone();
        false_truncation["context"]["retained_message_count"] =
            false_truncation["context"]["source_message_count"].clone();
        assert!(serde_json::from_value::<ManagedSessionRecord>(false_truncation).is_err());
        let mut empty = json;
        empty["context"]["retained_message_count"] = serde_json::Value::from(0);
        assert!(serde_json::from_value::<ManagedSessionRecord>(empty).is_err());

        let receipt = ResolvedSpawnReceipt {
            incarnation_id: NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            session: session_address("primary", 8),
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("primary").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            profile_revision: SpawnProfileRevision::new("default.r1").unwrap(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            prompt: SpawnPromptMetadata { present: false, byte_len: 0 },
            bundle_id: None,
            bundle: None,
            context_id: Some(context.id.clone()),
            context: Some(context),
            environment_profile: None,
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("spawn-1").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
            provenance: SpawnResolutionProvenance {
                provider: SpawnFieldProvenance::Profile,
                mode: SpawnFieldProvenance::Profile,
                terminal_size: SpawnFieldProvenance::Profile,
                prompt: SpawnFieldProvenance::Profile,
                bundle_id: SpawnFieldProvenance::Profile,
                context_id: SpawnFieldProvenance::Profile,
                environment_profile_id: SpawnFieldProvenance::Profile,
            },
            harness_mcp_proxy: None,
        };
        assert!(receipt.context_binding_is_valid());
        let mut mismatched = serde_json::to_value(&receipt).unwrap();
        mismatched["context_id"] = serde_json::Value::String("context-2".to_owned());
        assert!(serde_json::from_value::<ResolvedSpawnReceipt>(mismatched).is_err());
    }

    #[test]
    fn protocol_v9_workspace_and_worktree_mutations_have_exact_bounded_wire_shapes() {
        assert_eq!(NODE_PROTOCOL_VERSION, 11);
        assert_eq!(MAX_WORKSPACE_ROOT_BYTES, gate4agent_types::WORKING_DIRECTORY_MAX_BYTES);

        let register = NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            root: host_path(r"C:\repo-2"),
        };
        let register_json = serde_json::to_string(&register).unwrap();
        assert_eq!(
            register_json,
            r#"{"kind":"register-workspace","workspace_id":"repo-2","root":"C:\\repo-2"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&register_json).unwrap(), register);

        let standalone = NodeRequest::CreateStandaloneWorkspace {
            workspace_id: WorkspaceId::new("independent").unwrap(),
            root: host_path(r"C:\independent"),
            initial_branch: Some("main".to_owned()),
        };
        let standalone_json = serde_json::to_string(&standalone).unwrap();
        assert_eq!(
            standalone_json,
            r#"{"kind":"create-standalone-workspace","workspace_id":"independent","root":"C:\\independent","initial_branch":"main"}"#,
        );
        assert_eq!(
            serde_json::from_str::<NodeRequest>(&standalone_json).unwrap(),
            standalone,
        );
        assert_eq!(
            standalone.required_capability(),
            Some(NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY),
        );
        let oversized_branch = format!(
            r#"{{"kind":"create-standalone-workspace","workspace_id":"independent","root":"C:\\independent","initial_branch":"{}"}}"#,
            "x".repeat(MAX_REPOSITORY_PATH_BYTES + 1),
        );
        assert!(serde_json::from_str::<NodeRequest>(&oversized_branch).is_err());

        let unregister = NodeRequest::UnregisterWorkspace {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
        };
        let unregister_json = serde_json::to_string(&unregister).unwrap();
        assert_eq!(
            unregister_json,
            r#"{"kind":"unregister-workspace","workspace_id":"repo-2"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&unregister_json).unwrap(), unregister);

        let create = NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("topic-one").unwrap(),
            target_root: host_path(r"C:\trees\topic-one"),
            branch: "codex/topic-one".to_owned(),
            base: Some("main".to_owned()),
        };
        let create_json = serde_json::to_string(&create).unwrap();
        assert_eq!(
            create_json,
            r#"{"kind":"create-worktree","source_workspace_id":"primary","workspace_id":"topic-one","target_root":"C:\\trees\\topic-one","branch":"codex/topic-one","base":"main"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&create_json).unwrap(), create);

        let remove = NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            target_root: host_path(r"C:\trees\topic-one"),
        };
        let remove_json = serde_json::to_string(&remove).unwrap();
        assert_eq!(
            remove_json,
            r#"{"kind":"remove-worktree","source_workspace_id":"primary","target_root":"C:\\trees\\topic-one"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&remove_json).unwrap(), remove);
    }

    #[test]
    fn incarnation_id_and_cursor_have_exact_lowercase_hex_wire_shapes() {
        let incarnation_id = NodeIncarnationId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ]);
        assert_eq!(
            incarnation_id.to_string(),
            "00112233445566778899aabbccddeeff",
        );
        let json = serde_json::to_string(&incarnation_id).unwrap();
        assert_eq!(json, r#""00112233445566778899aabbccddeeff""#);
        assert_eq!(
            serde_json::from_str::<NodeIncarnationId>(&json).unwrap(),
            incarnation_id,
        );
        assert!("00112233445566778899AABBCCDDEEFF"
            .parse::<NodeIncarnationId>()
            .is_err());
        assert!("00112233445566778899aabbccddeef"
            .parse::<NodeIncarnationId>()
            .is_err());
        assert!("00112233445566778899aabbccddeefg"
            .parse::<NodeIncarnationId>()
            .is_err());

        let cursor = NodeCursor {
            incarnation_id,
            sequence: 17,
        };
        let cursor_json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(
            cursor_json,
            r#"{"incarnation_id":"00112233445566778899aabbccddeeff","sequence":17}"#,
        );
        assert_eq!(serde_json::from_str::<NodeCursor>(&cursor_json).unwrap(), cursor);
    }

    #[test]
    fn node_hello_v8_carries_the_incarnation_sequence_domain() {
        let hello = NodeHello {
            protocol_version: NODE_PROTOCOL_VERSION,
            incarnation_id: NodeIncarnationId::from_bytes([0; NODE_INCARNATION_ID_BYTES]),
            connection_id: 42,
            role: ClientRole::Observer,
            event_sequence: 9,
            controller: None,
            snapshot: NodeSnapshot {
                node_id: NodeId::new("fixture-node").unwrap(),
                enabled_providers: Vec::new(),
                provider_runtime_statuses: ProviderRuntimeStatuses::default(),
                workspaces: Vec::new(),
                session_records: Vec::new(),
                managed_worktrees: Vec::new(),
                launch_inventory: None,
                agent_progress: Vec::new(),
            },
            compatibility: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":9,"incarnation_id":"00000000000000000000000000000000","connection_id":42,"role":"observer","event_sequence":9,"controller":null,"snapshot":{"node_id":"fixture-node","enabled_providers":[],"workspaces":[],"session_records":[]}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeHello>(&json).unwrap(), hello);
    }

    #[test]
    fn agent_progress_v1_rejects_oversize_and_controls() {
        let address = serde_json::json!({
            "workspace_id": "primary",
            "session": { "instance_id": 7, "generation": 3 }
        });
        let valid_progress = serde_json::json!({
            "provider_sequence": 11,
            "activity": "working",
            "completed_turns": 2,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_tokens": 30,
                "cache_write_tokens": 40,
                "reasoning_tokens": 50
            },
            "current": "working",
            "active_tool_labels": ["Read"],
            "active_tool_count": 1,
            "attention": null,
            "subagent_count": 0,
            "last_event_kind": "tool-started",
            "gap_count": 0,
            "stale": false,
            "truncated": false
        });
        let mut controlled = valid_progress.clone();
        controlled["active_tool_labels"] = serde_json::json!(["unsafe\u{0000}tool"]);
        assert!(serde_json::from_value::<AgentProgressV1>(controlled.clone()).is_err());

        let mut path_like = valid_progress.clone();
        path_like["active_tool_labels"] = serde_json::json!([r"Read C:\private\secret"]);
        assert!(serde_json::from_value::<AgentProgressV1>(path_like).is_err());

        let mut oversized_label = valid_progress.clone();
        oversized_label["active_tool_labels"] =
            serde_json::json!(["x".repeat(MAX_AGENT_PROGRESS_TOOL_LABEL_BYTES + 1)]);
        assert!(serde_json::from_value::<AgentProgressV1>(oversized_label.clone()).is_err());

        let oversized_entry = serde_json::json!({
            "address": address.clone(),
            "progress": valid_progress,
            "padding": "x".repeat(MAX_AGENT_PROGRESS_ENTRY_BYTES)
        });
        assert!(serde_json::to_vec(&oversized_entry).unwrap().len()
            > MAX_AGENT_PROGRESS_ENTRY_BYTES);
        let snapshot = serde_json::json!({
            "node_id": "fixture-node",
            "enabled_providers": [],
            "workspaces": [],
            "session_records": [],
            "agent_progress": [
                { "address": address.clone(), "progress": controlled },
                { "address": address, "progress": oversized_label },
                oversized_entry
            ]
        });
        let decoded = serde_json::from_value::<NodeSnapshot>(snapshot).unwrap();
        assert!(decoded.agent_progress.is_empty());
    }

    #[test]
    fn launch_inventory_preserves_legacy_absence_and_authoritative_empty() {
        let legacy = serde_json::from_str::<NodeSnapshot>(
            r#"{"node_id":"fixture-node","enabled_providers":[],"workspaces":[],"session_records":[]}"#,
        )
        .unwrap();
        assert!(legacy.launch_inventory.is_none());
        assert!(serde_json::from_str::<LaunchInventory>(r#"{}"#).is_err());

        let current = LaunchInventory {
            spawn_profiles: Some(Vec::new()),
            bundles: Some(Vec::new()),
        };
        assert_eq!(
            serde_json::to_string(&current).unwrap(),
            r#"{"spawn_profiles":[],"bundles":[]}"#,
        );
        assert_eq!(
            serde_json::from_str::<LaunchInventory>(
                r#"{"spawn_profiles":[],"bundles":[]}"#,
            )
            .unwrap(),
            current,
        );

        let legacy_workspace = serde_json::from_str::<WorkspaceSnapshot>(
            r#"{"workspace_id":"repo","canonical_root":"fixture-root","sessions":[]}"#,
        )
        .unwrap();
        assert!(legacy_workspace.managed_worktree_profiles.is_none());
        let current_workspace = WorkspaceSnapshot {
            managed_worktree_profiles: Some(WorktreeProfileInventory {
                profiles: Vec::new(),
            }),
            ..legacy_workspace
        };
        assert!(serde_json::to_string(&current_workspace)
            .unwrap()
            .contains(r#""managed_worktree_profiles":[]"#));
    }

    #[test]
    fn launch_inventory_rejects_duplicate_and_overflow_identities() {
        let duplicate_profiles = r#"{"spawn_profiles":[{"id":"default","revision":"v1"},{"id":"default","revision":"v2"}]}"#;
        assert!(serde_json::from_str::<LaunchInventory>(duplicate_profiles).is_err());

        let digest = format!("sha256:{}", "0".repeat(64));
        let duplicate_bundles = serde_json::json!({
            "bundles": [
                { "id": "review", "revision": "v1", "digest": digest },
                { "id": "review", "revision": "v2", "digest": format!("sha256:{}", "1".repeat(64)) },
            ],
        });
        assert!(serde_json::from_value::<LaunchInventory>(duplicate_bundles).is_err());

        let duplicate_worktrees = r#"[{"id":"default","revision":"v1","retention":"retain"},{"id":"default","revision":"v2","retention":"remove-when-released"}]"#;
        assert!(serde_json::from_str::<WorktreeProfileInventory>(duplicate_worktrees).is_err());

        let profiles = (0..=MAX_SPAWN_PROFILES)
            .map(|index| serde_json::json!({
                "id": format!("profile-{index}"),
                "revision": "v1",
            }))
            .collect::<Vec<_>>();
        let overflow = serde_json::json!({ "spawn_profiles": profiles });
        assert!(serde_json::from_value::<LaunchInventory>(overflow).is_err());

        let bundles = (0..=MAX_LAUNCH_BUNDLES)
            .map(|index| serde_json::json!({
                "id": format!("bundle-{index}"),
                "revision": "v1",
                "digest": format!("sha256:{}", "2".repeat(64)),
            }))
            .collect::<Vec<_>>();
        assert!(serde_json::from_value::<LaunchInventory>(
            serde_json::json!({ "bundles": bundles }),
        )
        .is_err());

        let worktrees = (0..=MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE)
            .map(|index| serde_json::json!({
                "id": format!("profile-{index}"),
                "revision": "v1",
                "retention": "retain",
            }))
            .collect::<Vec<_>>();
        assert!(serde_json::from_value::<WorktreeProfileInventory>(
            serde_json::Value::Array(worktrees),
        )
        .is_err());
    }

    #[test]
    fn managed_worktree_contract_is_bounded_dual_gated_and_path_free() {
        assert_eq!(
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
            "managed-worktree-lifecycle-v1",
        );
        assert_eq!(NODE_PROTOCOL_VERSION, 11);
        assert_eq!(NODE_STATE_SCHEMA_V4, 4);
        assert_eq!(NODE_STATE_SCHEMA_V5, 5);
        assert_eq!(NODE_STATE_SCHEMA_V6, 6);
        assert_eq!(NODE_STATE_SCHEMA_V10, 10);
        assert!(WorktreeProfileId::new("p".repeat(MAX_WORKTREE_PROFILE_ID_BYTES)).is_ok());
        assert!(WorktreeProfileId::new("p".repeat(MAX_WORKTREE_PROFILE_ID_BYTES + 1)).is_err());
        assert!(WorktreeProfileRevision::new(
            "r".repeat(MAX_WORKTREE_PROFILE_REVISION_BYTES),
        )
        .is_ok());
        assert!(ManagedWorktreeLeaseId::new(
            "l".repeat(MAX_MANAGED_WORKTREE_LEASE_ID_BYTES + 1),
        )
        .is_err());

        let request = NodeRequest::SpawnManagedWorktree {
            request: ManagedWorktreeSpawnRequest {
                spawn_spec: SpawnSpec {
                    target: SpawnTarget {
                        node_id: NodeId::new("node-a").unwrap(),
                        workspace_id: WorkspaceId::new("primary").unwrap(),
                        worktree_id: None,
                    },
                    profile_id: SpawnProfileId::new("default").unwrap(),
                    expected_profile_revision: SpawnProfileRevision::new("default.r1").unwrap(),
                    overrides: SpawnOverrides::default(),
                    deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
                    idempotency_key: SpawnIdempotencyKey::new("managed-1").unwrap(),
                    required_capabilities: SpawnRequiredCapabilities::default(),
                },
                worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
            },
        };
        assert_eq!(
            request.required_capability(),
            Some(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY),
        );
        assert!(request.requires_worktree_selection_capability());
        let json = serde_json::to_string(&request).unwrap();
        for forbidden in ["canonical", "target_root", "gitdir", "branch", "base_commit", "diagnostic"] {
            assert!(!json.contains(forbidden), "managed request leaked {forbidden}");
        }

        let legacy = match request {
            NodeRequest::SpawnManagedWorktree { request } => request,
            _ => unreachable!(),
        };
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        assert!(serde_json::from_str::<ManagedWorktreeSpawnRequest>(&legacy_json).is_ok());
        assert!(serde_json::from_str::<ManagedWorktreeSpawnRequestV2>(&legacy_json).is_err());
        let v2 = NodeRequest::SpawnManagedWorktreeV2 {
            request: ManagedWorktreeSpawnRequestV2 {
                spawn_spec: legacy.spawn_spec,
                worktree_profile_id: legacy.worktree_profile_id,
                expected_profile_revision: WorktreeProfileRevision::new("review.r1").unwrap(),
            },
        };
        assert_eq!(
            v2.required_capability(),
            Some(NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY),
        );
        assert!(v2.requires_worktree_selection_capability());
    }

    #[test]
    fn managed_worktree_snapshot_rejects_invalid_time_duplicate_identity_and_overflow() {
        fn lease(lease_id: &str, workspace_id: &str) -> serde_json::Value {
            serde_json::json!({
                "lease_id": lease_id,
                "source_workspace_id": "primary",
                "workspace_id": workspace_id,
                "profile_id": "review",
                "profile_revision": "review.r1",
                "retention": "remove-when-released",
                "state": "ready",
                "active_session_count": 0,
                "managed_record_count": 0,
                "cleanup_failure": null,
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 2
            })
        }
        fn snapshot(leases: Vec<serde_json::Value>) -> serde_json::Value {
            serde_json::json!({
                "node_id": "node-a",
                "enabled_providers": [],
                "workspaces": [],
                "session_records": [],
                "managed_worktrees": leases
            })
        }

        let mut reversed = lease("lease-a", "managed-a");
        reversed["updated_at_unix_ms"] = serde_json::json!(0);
        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![reversed])).is_err());
        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![lease(
            "lease-a",
            "primary",
        )]))
        .is_err());

        for (state, active, records, failure) in [
            ("ready", 1, 0, serde_json::Value::Null),
            ("in-use", 0, 0, serde_json::Value::Null),
            ("in-use", 1, 0, serde_json::json!("busy")),
            ("cleanup-blocked", 0, 0, serde_json::Value::Null),
            ("cleanup-blocked", 1, 0, serde_json::json!("busy")),
            ("recovery-required", 0, 0, serde_json::Value::Null),
            ("removed", 0, 1, serde_json::Value::Null),
        ] {
            let mut malformed = lease("lease-a", "managed-a");
            malformed["state"] = serde_json::json!(state);
            malformed["active_session_count"] = serde_json::json!(active);
            malformed["managed_record_count"] = serde_json::json!(records);
            malformed["cleanup_failure"] = failure;
            assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![malformed])).is_err());
        }
        let mut recovery_with_holder = lease("lease-a", "managed-a");
        recovery_with_holder["state"] = serde_json::json!("recovery-required");
        recovery_with_holder["managed_record_count"] = serde_json::json!(1);
        recovery_with_holder["cleanup_failure"] = serde_json::json!("ownership-conflict");
        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![recovery_with_holder])).is_ok());

        let mut spawn_lease = lease("lease-a", "managed-a");
        spawn_lease["state"] = serde_json::json!("in-use");
        spawn_lease["active_session_count"] = serde_json::json!(1);
        spawn_lease["managed_record_count"] = serde_json::json!(1);
        let spawn = serde_json::json!({
            "incarnation_id": "00000000000000000000000000000000",
            "session": {
                "workspace_id": "managed-a",
                "session": { "instance_id": 1, "generation": 1 }
            },
            "target": {
                "node_id": "node-a",
                "workspace_id": "primary",
                "worktree_id": "managed-a"
            },
            "profile_id": "default",
            "profile_revision": "default.r1",
            "provider": "claude",
            "mode": "pty",
            "terminal_size": { "rows": 24, "columns": 80 },
            "prompt": { "present": false, "byte_len": 0 },
            "bundle_id": null,
            "context_id": null,
            "environment_profile_id": null,
            "deadline_ms": 5000,
            "idempotency_key": "managed-1",
            "required_capabilities": [],
            "provenance": {
                "provider": "profile",
                "mode": "profile",
                "terminal_size": "profile",
                "prompt": "profile",
                "bundle_id": "profile",
                "context_id": "profile",
                "environment_profile_id": "profile"
            }
        });
        let valid_receipt = serde_json::json!({ "spawn": spawn, "lease": spawn_lease });
        assert!(serde_json::from_value::<ManagedWorktreeSpawnReceipt>(valid_receipt.clone()).is_ok());
        let mut wrong_source = valid_receipt.clone();
        wrong_source["lease"]["source_workspace_id"] = serde_json::json!("other");
        assert!(serde_json::from_value::<ManagedWorktreeSpawnReceipt>(wrong_source).is_err());
        let mut wrong_session = valid_receipt;
        wrong_session["spawn"]["session"]["workspace_id"] = serde_json::json!("other");
        assert!(serde_json::from_value::<ManagedWorktreeSpawnReceipt>(wrong_session).is_err());

        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![
            lease("lease-a", "managed-a"),
            lease("lease-a", "managed-b"),
        ]))
        .is_err());
        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(vec![
            lease("lease-a", "managed-a"),
            lease("lease-b", "managed-a"),
        ]))
        .is_err());

        let leases = (0..=MAX_MANAGED_WORKTREE_LEASES)
            .map(|index| lease(&format!("lease-{index}"), &format!("managed-{index}")))
            .collect();
        assert!(serde_json::from_value::<NodeSnapshot>(snapshot(leases)).is_err());
    }

    #[test]
    fn terminal_bytes_round_trip_as_an_exact_byte_array() {
        let request = NodeRequest::TerminalBytes {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                },
            },
            bytes: b"\x1b[1;5D".to_vec(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"terminal-bytes","session":{"workspace_id":"primary","session":{"instance_id":7,"generation":3}},"bytes":[27,91,49,59,53,68]}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
    }

    #[test]
    fn terminal_frame_event_wire_contract_is_exact() {
        let event = NodeEvent::TerminalFrame {
            address: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                },
            },
            frame: TerminalFrame {
                sequence: 11,
                size: TerminalSize { rows: 24, columns: 80 },
                cursor_row: 2,
                cursor_column: 4,
                contents: "ready".to_owned(),
                formatted: b"ready".to_vec(),
                scrollback_formatted: vec![b"previous".to_vec()],
                alternate_screen: false,
                mouse_protocol_enabled: false,
                mouse_protocol_encoding:
                    gate4agent_types::TerminalMouseProtocolEncoding::Default,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"terminal-frame","address":{"workspace_id":"primary","session":{"instance_id":7,"generation":3}},"frame":{"sequence":11,"size":{"rows":24,"columns":80},"cursor_row":2,"cursor_column":4,"contents":"ready","formatted":[114,101,97,100,121],"scrollback_formatted":[[112,114,101,118,105,111,117,115]],"alternate_screen":false,"mouse_protocol_enabled":false,"mouse_protocol_encoding":"default"}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeEvent>(&json).unwrap(), event);
    }

    #[test]
    fn managed_observation_requires_managed_target_capability() {
        assert_eq!(
            NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
            "observation-managed-target-v1",
        );
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| {
                capability.as_str() == NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY
            }));
        let event = NodeEvent::ManagedObservation {
            record_id: SessionRecordId::new("record-a").unwrap(),
            observation: ObservationV1 {
                source_sequence: 3,
                observed_at_unix_ms: Some(2),
                evidence: ObservationEvidenceV1::StructuredProvider,
                kind: ObservationKindV1::Working,
                truncated: false,
            },
        };
        assert!(event.requires_observation_events_capability());
        assert!(event.requires_observation_managed_target_capability());
        assert!(!event.requires_observation_workflow_detail_capability());
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"managed-observation","record_id":"record-a","observation":{"source_sequence":3,"observed_at_unix_ms":2,"evidence":"structured-provider","kind":{"kind":"working"},"truncated":false}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeEvent>(&json).unwrap(), event);
    }

    #[test]
    fn terminal_frame_events_capability_is_optional_and_auth_bound_exactly() {
        assert_eq!(
            NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
            "terminal-frame-events-v1",
        );
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY));

        let capability = CapabilityId::new(NODE_TERMINAL_FRAME_EVENTS_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities = vec![capability.clone()];
        let legacy = ClientCompatibilityOffer::exact(NODE_PROTOCOL_VERSION).unwrap();
        assert!(support
            .negotiate(NODE_PROTOCOL_VERSION, &legacy)
            .unwrap()
            .capabilities
            .is_empty());

        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = support
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(selected.capabilities, vec![capability]);
        assert_eq!(
            String::from_utf8(
                encode_node_compatibility_auth_binding(&offer, &selected).unwrap(),
            )
            .unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":9,"maximum":9},"capabilities":["terminal-frame-events-v1"]},"selected":{"protocol_version":9,"capabilities":["terminal-frame-events-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[]}}"#,
        );
    }

    #[test]
    fn child_environment_profile_capability_is_optional_and_auth_bound_exactly() {
        assert_eq!(
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
            "child-environment-profile-v1",
        );
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| {
                capability.as_str() == NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY
            }));

        let capability =
            CapabilityId::new(NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities = vec![capability.clone()];
        let legacy = ClientCompatibilityOffer::exact(NODE_PROTOCOL_VERSION).unwrap();
        assert!(support
            .negotiate(NODE_PROTOCOL_VERSION, &legacy)
            .unwrap()
            .capabilities
            .is_empty());

        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = support
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(selected.capabilities, vec![capability]);
        let bound = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert!(bound
            .windows(NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY.len())
            .any(|window| {
                window == NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY.as_bytes()
            }));
    }

    #[test]
    fn worktree_selection_capability_is_optional_and_auth_bound_exactly() {
        assert_eq!(NODE_WORKTREE_SELECTION_CAPABILITY, "worktree-selection-v1");
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == NODE_WORKTREE_SELECTION_CAPABILITY));

        let capability = CapabilityId::new(NODE_WORKTREE_SELECTION_CAPABILITY).unwrap();
        let mut support = portable_node_support();
        support.capabilities = vec![capability.clone()];
        let legacy = ClientCompatibilityOffer::exact(NODE_PROTOCOL_VERSION).unwrap();
        assert!(support
            .negotiate(NODE_PROTOCOL_VERSION, &legacy)
            .unwrap()
            .capabilities
            .is_empty());

        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = support.negotiate(NODE_PROTOCOL_VERSION, &offer).unwrap();
        assert_eq!(selected.capabilities, vec![capability]);
        assert_eq!(
            String::from_utf8(
                encode_node_compatibility_auth_binding(&offer, &selected).unwrap(),
            )
            .unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":9,"maximum":9},"capabilities":["worktree-selection-v1"]},"selected":{"protocol_version":9,"capabilities":["worktree-selection-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[]}}"#,
        );
    }

    #[test]
    fn legacy_node_event_bytes_remain_exact_after_terminal_frame_addition() {
        assert_eq!(
            serde_json::to_vec(&NodeEvent::ResyncRequired {
                oldest_available_sequence: 7,
            })
            .unwrap(),
            br#"{"kind":"resync-required","oldest_available_sequence":7}"#,
        );
    }

    #[test]
    fn workspace_responses_and_events_round_trip_without_client_only_state() {
        let workspace = WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            canonical_root: host_path(r"C:\repo-2"),
            sessions: Vec::new(),
            worktree_service_mode: None,
            managed_worktree_profiles: None,
        };
        let registered = NodeResponse::WorkspaceRegistered {
            workspace: workspace.clone(),
        };
        let registered_json = serde_json::to_string(&registered).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeResponse>(&registered_json).unwrap(),
            registered,
        );
        let standalone = NodeResponse::StandaloneWorkspaceCreated {
            workspace: workspace.clone(),
        };
        let standalone_json = serde_json::to_string(&standalone).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeResponse>(&standalone_json).unwrap(),
            standalone,
        );
        let added = NodeEventEnvelope {
            sequence: 19,
            event: NodeEvent::WorkspaceAdded {
                workspace: workspace.clone(),
            },
        };
        let added_json = serde_json::to_string(&added).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeEventEnvelope>(&added_json).unwrap(),
            added,
        );
        let removed = NodeEventEnvelope {
            sequence: 20,
            event: NodeEvent::WorkspaceRemoved {
                workspace_id: workspace.workspace_id.clone(),
            },
        };
        let removed_json = serde_json::to_string(&removed).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeEventEnvelope>(&removed_json).unwrap(),
            removed,
        );

        let created = NodeResponse::WorktreeCreated {
            worktree: GitWorktreeSnapshot {
                path: host_path(r"C:\trees\topic-one"),
                head: "abc1234".to_owned(),
                branch: Some("codex/topic-one".to_owned()),
                is_bare: false,
                is_main: false,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: Some(workspace.workspace_id.clone()),
            },
            workspace: workspace.clone(),
        };
        let created_json = serde_json::to_string(&created).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&created_json).unwrap(), created);
        let removed = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\trees\topic-one"),
            workspace_id: Some(workspace.workspace_id),
        };
        let removed_json = serde_json::to_string(&removed).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&removed_json).unwrap(), removed);
    }

    #[test]
    fn workspace_inspection_round_trips_as_a_workspace_scoped_read_only_request() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let request = NodeRequest::InspectWorkspace {
            workspace_id: workspace_id.clone(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            request_json,
            r#"{"kind":"inspect-workspace","workspace_id":"primary"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&request_json).unwrap(), request);

        let response = NodeResponse::WorkspaceInspected {
            inspection: WorkspaceInspection {
                workspace_id,
                entries: vec![
                    WorkspaceEntry {
                        relative_path: repository_path("src"),
                        kind: WorkspaceEntryKind::Directory,
                    },
                    WorkspaceEntry {
                        relative_path: repository_path("src/lib.rs"),
                        kind: WorkspaceEntryKind::File,
                    },
                ],
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: true,
                    branch: Some("main".to_owned()),
                    status: vec![GitStatusEntry {
                        index_status: " ".to_owned(),
                        worktree_status: "M".to_owned(),
                        path: repository_path("src/lib.rs"),
                        previous_path: None,
                    }],
                    recent_commits: vec![GitCommitSummary {
                        id: "abc1234".to_owned(),
                        summary: "bounded summary".to_owned(),
                    }],
                    worktrees: vec![GitWorktreeSnapshot {
                        path: host_path(r"C:\repo"),
                        head: "abc1234".to_owned(),
                        branch: Some("main".to_owned()),
                        is_bare: false,
                        is_main: true,
                        locked: false,
                        lock_reason: None,
                        prunable: false,
                        prunable_reason: None,
                        workspace_id: Some(WorkspaceId::new("primary").unwrap()),
                    }],
                    managed_worktree: None,
                    truncated: false,
                    diagnostic: None,
                },
            },
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert!(!response_json.contains("managed_worktree"));
        assert_eq!(serde_json::from_str::<NodeResponse>(&response_json).unwrap(), response);
    }

    #[test]
    fn managed_worktree_git_scope_is_optional_bounded_and_round_trips() {
        let scope = ManagedWorktreeGitScope {
            lease_id: ManagedWorktreeLeaseId::new("mw-scope").unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            branch: "gate4agent/mw-scope".to_owned(),
            base_commit: GitObjectId::new(
                "0123456789abcdef0123456789abcdef01234567".to_owned(),
            )
            .unwrap(),
            active_session_count: 1,
            managed_record_count: 1,
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(serde_json::from_str::<ManagedWorktreeGitScope>(&json).unwrap(), scope);

        let oversized = format!(
            r#"{{"lease_id":"mw-scope","source_workspace_id":"primary","branch":"{}","base_commit":"0123456789abcdef0123456789abcdef01234567","active_session_count":0,"managed_record_count":0}}"#,
            "x".repeat(MAX_REPOSITORY_PATH_BYTES + 1),
        );
        assert!(serde_json::from_str::<ManagedWorktreeGitScope>(&oversized).is_err());
    }

    #[test]
    fn workspace_inspection_rejects_inconsistent_managed_git_scope() {
        let valid = r#"{"workspace_id":"managed-a","entries":[],"tree_truncated":false,"git":{"is_repository":true,"branch":"gate4agent/a","status":[],"recent_commits":[],"worktrees":[],"managed_worktree":{"lease_id":"mw-a","source_workspace_id":"primary","branch":"gate4agent/a","base_commit":"0123456789abcdef0123456789abcdef01234567","active_session_count":1,"managed_record_count":0},"truncated":false,"diagnostic":null}}"#;
        assert!(serde_json::from_str::<WorkspaceInspection>(valid).is_ok());
        for invalid in [
            valid.replace("\"is_repository\":true", "\"is_repository\":false"),
            valid.replacen("\"branch\":\"gate4agent/a\"", "\"branch\":\"gate4agent/b\"", 1),
            valid.replace("\"source_workspace_id\":\"primary\"", "\"source_workspace_id\":\"managed-a\""),
            valid.replace("\"active_session_count\":1", "\"active_session_count\":0"),
        ] {
            assert!(serde_json::from_str::<WorkspaceInspection>(&invalid).is_err());
        }
    }

    #[test]
    fn workspace_file_read_has_an_exact_capability_gated_wire_contract() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let path = repository_path("src/lib.rs");
        let request = NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: path.clone(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            request_json,
            r#"{"kind":"read-workspace-file","workspace_id":"primary","path":"src/lib.rs"}"#,
        );
        assert_eq!(request.required_capability(), Some(NODE_WORKSPACE_FILE_READ_CAPABILITY));
        assert_eq!(serde_json::from_str::<NodeRequest>(&request_json).unwrap(), request);

        let response = NodeResponse::WorkspaceFileRead {
            file: WorkspaceFileRead {
                workspace_id,
                path,
                content: WorkspaceFileContent::Utf8 {
                    text: "fn main() {}\n".to_owned(),
                    byte_len: 13,
                },
                revision: None,
            },
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            response_json,
            r#"{"kind":"workspace-file-read","file":{"workspace_id":"primary","path":"src/lib.rs","content":{"kind":"utf8","text":"fn main() {}\n","byte_len":13}}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeResponse>(&response_json).unwrap(), response);
    }

    #[test]
    fn workspace_entry_create_has_exact_capability_gated_wire_contracts() {
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY));
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let file_path = repository_path("src/new.rs");
        let directory_path = repository_path("src/new");
        let create_file = NodeRequest::CreateWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: file_path.clone(),
        };
        let create_directory = NodeRequest::CreateWorkspaceDirectory {
            workspace_id: workspace_id.clone(),
            path: directory_path.clone(),
        };
        assert_eq!(
            serde_json::to_string(&create_file).unwrap(),
            r#"{"kind":"create-workspace-file","workspace_id":"primary","path":"src/new.rs"}"#,
        );
        assert_eq!(
            serde_json::to_string(&create_directory).unwrap(),
            r#"{"kind":"create-workspace-directory","workspace_id":"primary","path":"src/new"}"#,
        );
        for request in [create_file, create_directory] {
            assert_eq!(
                request.required_capability(),
                Some(NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY),
            );
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<NodeRequest>(&encoded).unwrap(), request);
        }

        let file_response = NodeResponse::WorkspaceFileCreated {
            file: WorkspaceFileRead {
                workspace_id: workspace_id.clone(),
                path: file_path,
                content: WorkspaceFileContent::Utf8 {
                    text: String::new(),
                    byte_len: 0,
                },
                revision: Some(WorkspaceFileRevision::new("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned()).unwrap()),
            },
        };
        let directory_response = NodeResponse::WorkspaceDirectoryCreated {
            workspace_id,
            entry: WorkspaceEntry {
                relative_path: directory_path,
                kind: WorkspaceEntryKind::Directory,
            },
        };
        for response in [file_response, directory_response] {
            let encoded = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<NodeResponse>(&encoded).unwrap(), response);
        }
    }

    #[test]
    fn workspace_file_read_worst_case_json_stays_within_the_server_frame_limit() {
        let response = NodeResponse::WorkspaceFileRead {
            file: WorkspaceFileRead {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                path: repository_path(&"x".repeat(MAX_REPOSITORY_PATH_BYTES)),
                content: WorkspaceFileContent::Utf8 {
                    text: "\0".repeat(MAX_WORKSPACE_FILE_BYTES),
                    byte_len: u32::try_from(MAX_WORKSPACE_FILE_BYTES).unwrap(),
                },
                revision: None,
            },
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= MAX_NODE_FRAME_BYTES, "{}", encoded.len());
    }

    #[test]
    fn workspace_write_and_git_reads_have_exact_capability_gated_contracts() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let path = repository_path("src/lib.rs");
        let revision = WorkspaceFileRevision::new("a".repeat(64)).unwrap();
        let write = NodeRequest::WriteWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            expected_revision: revision,
            text: "updated\n".to_owned(),
        };
        assert_eq!(write.required_capability(), Some(NODE_WORKSPACE_FILE_WRITE_CAPABILITY));
        let encoded = serde_json::to_string(&write).unwrap();
        assert_eq!(serde_json::from_str::<NodeRequest>(&encoded).unwrap(), write);

        let history = NodeRequest::ReadGitHistory {
            workspace_id: workspace_id.clone(),
            path: Some(path.clone()),
            before: Some(GitObjectId::new("b".repeat(40)).unwrap()),
            limit: MAX_GIT_HISTORY_COMMITS,
        };
        let diff = NodeRequest::ReadGitDiff {
            workspace_id,
            request: GitDiffRequest {
                mode: GitDiffMode::Commit {
                    revision: GitObjectId::new("c".repeat(40)).unwrap(),
                },
                path: Some(path),
            },
        };
        for request in [history, diff] {
            assert_eq!(request.required_capability(), Some(NODE_GIT_READ_CAPABILITY));
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<NodeRequest>(&encoded).unwrap(), request);
        }
    }

    #[test]
    fn workspace_file_content_deserialization_rejects_unbounded_or_inconsistent_lengths() {
        let valid = [
            WorkspaceFileContent::Utf8 {
                text: "тест".to_owned(),
                byte_len: 8,
            },
            WorkspaceFileContent::NonUtf8 { byte_len: 17 },
            WorkspaceFileContent::TooLarge {
                limit_bytes: MAX_WORKSPACE_FILE_BYTES as u32,
            },
        ];
        for content in valid {
            let encoded = serde_json::to_string(&content).unwrap();
            assert_eq!(
                serde_json::from_str::<WorkspaceFileContent>(&encoded).unwrap(),
                content,
            );
        }

        let inconsistent = r#"{"kind":"utf8","text":"hello","byte_len":4}"#;
        assert!(serde_json::from_str::<WorkspaceFileContent>(inconsistent).is_err());

        let oversized_non_utf8 = format!(
            r#"{{"kind":"non-utf8","byte_len":{}}}"#,
            MAX_WORKSPACE_FILE_BYTES + 1,
        );
        assert!(serde_json::from_str::<WorkspaceFileContent>(&oversized_non_utf8).is_err());

        let wrong_limit = r#"{"kind":"too-large","limit_bytes":1}"#;
        assert!(serde_json::from_str::<WorkspaceFileContent>(wrong_limit).is_err());

        let oversized_utf8 = WorkspaceFileContent::Utf8 {
            text: "x".repeat(MAX_WORKSPACE_FILE_BYTES + 1),
            byte_len: u32::try_from(MAX_WORKSPACE_FILE_BYTES + 1).unwrap(),
        };
        let encoded = serde_json::to_string(&oversized_utf8).unwrap();
        assert!(serde_json::from_str::<WorkspaceFileContent>(&encoded).is_err());
    }

    #[test]
    fn legacy_requests_do_not_acquire_a_new_required_capability() {
        let legacy_requests = [
            NodeRequest::Snapshot,
            NodeRequest::Resync { after_sequence: 7 },
            NodeRequest::InspectWorkspace {
                workspace_id: WorkspaceId::new("primary").unwrap(),
            },
        ];
        for request in legacy_requests {
            assert_eq!(request.required_capability(), None);
        }
    }

    #[test]
    fn host_directory_browse_wire_contract_is_bounded_and_capability_gated() {
        assert!(production_node_client_compatibility_offer()
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == CAPABILITY_HOST_DIRECTORY_BROWSE_V1));
        let directory = OpaqueHostPath::utf8(r"C:\repo".to_owned()).unwrap();
        let request = NodeRequest::BrowseHostDirectories {
            directory: Some(directory.clone()),
            after: None,
        };
        assert_eq!(
            request.required_capability(),
            Some(CAPABILITY_HOST_DIRECTORY_BROWSE_V1),
        );
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"kind":"browse-host-directories","directory":"C:\\repo","after":null}"#,
        );

        let response = NodeResponse::HostDirectoriesBrowsed {
            listing: HostDirectoryListing {
                directory: Some(directory.clone()),
                parent: Some(OpaqueHostPath::utf8(r"C:\".to_owned()).unwrap()),
                entries: vec![HostDirectoryEntry {
                    path: OpaqueHostPath::utf8(r"C:\repo\child".to_owned()).unwrap(),
                    display_name: "child".to_owned(),
                    is_link: false,
                }],
                next_after: None,
                incomplete: false,
            },
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&encoded).unwrap(), response);

        let entries = (0..=MAX_HOST_DIRECTORY_ENTRIES)
            .map(|index| serde_json::json!({
                "path": format!(r"C:\directory-{index}"),
                "display_name": format!("directory-{index}"),
                "is_link": false,
            }))
            .collect::<Vec<_>>();
        let overflow = serde_json::json!({
            "directory": null,
            "parent": null,
            "entries": entries,
            "next_after": null,
            "incomplete": true,
        });
        assert!(serde_json::from_value::<HostDirectoryListing>(overflow).is_err());
        assert_eq!(
            serde_json::to_string(&NodeFailureCode::HostDirectoryReadTimedOut).unwrap(),
            r#""host-directory-read-timed-out""#,
        );
    }

    #[test]
    fn host_directory_entry_rejects_unbounded_control_or_non_utf8_wire_values() {
        let path = host_path(r"C:\repo\child");
        assert!(HostDirectoryEntry::new(path.clone(), "child".to_owned(), false).is_ok());
        for display_name in [
            String::new(),
            "child\nname".to_owned(),
            "x".repeat(MAX_HOST_DIRECTORY_DISPLAY_NAME_BYTES + 1),
        ] {
            let encoded = serde_json::json!({
                "path": path,
                "display_name": display_name,
                "is_link": false,
            });
            assert!(serde_json::from_value::<HostDirectoryEntry>(encoded).is_err());
        }
        let non_utf8 = serde_json::json!({
            "path": { "kind": "unix-bytes", "bytes": [47, 255] },
            "display_name": "child",
            "is_link": false,
        });
        assert!(serde_json::from_value::<HostDirectoryEntry>(non_utf8).is_err());
    }

    #[test]
    fn git_snapshot_defaults_worktrees_for_legacy_inspection_payloads() {
        let json = r#"{"is_repository":true,"branch":"main","status":[],"recent_commits":[],"truncated":false,"diagnostic":null}"#;
        let snapshot = serde_json::from_str::<GitSnapshot>(json).unwrap();
        assert!(snapshot.worktrees.is_empty());
    }

    #[test]
    fn promptless_resume_round_trips_as_null() {
        let request = NodeRequest::Resume {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains(r#""initial_prompt":null"#));
        assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
    }

    #[test]
    fn node_and_workspace_ids_are_bounded_validated_wire_values() {
        assert_eq!(NodeId::new("node-1").unwrap().as_str(), "node-1");
        assert_eq!(WorkspaceId::new("repo_main").unwrap().as_str(), "repo_main");
        assert!(NodeId::new("Node-1").is_err());
        assert!(WorkspaceId::new("-repo").is_err());
        assert!(WorkspaceId::new("x".repeat(MAX_NODE_IDENTIFIER_BYTES + 1)).is_err());

        let encoded = serde_json::to_string(&WorkspaceId::new("repo-1").unwrap()).unwrap();
        assert_eq!(encoded, "\"repo-1\"");
        assert!(serde_json::from_str::<WorkspaceId>("\"Repo-1\"").is_err());
    }

    #[test]
    fn durable_session_wire_contract_round_trips() {
        assert_eq!(MAX_SESSION_DISPLAY_NAME_BYTES, 256);
        let record = ManagedSessionRecord {
            record_id: SessionRecordId::new("session-001").unwrap(),
            display_name: "release shepherd".to_owned(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: host_path(r"C:\repo"),
            provider_session: Some(ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: "b1ef3250-47a2-42ca-9076-cc241487ea22".to_owned(),
                transcript_path: Some(r"C:\provider\sessions\b1ef3250.jsonl".to_owned()),
            }),
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            created_at_unix_ms: 1_723_000_000_000,
            updated_at_unix_ms: 1_723_000_000_123,
            last_error: None,
        };
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("environment_profile"));

        let native_selection = NativeSessionSelection {
            route: NativeSessionCatalogRoute::workspace(
                record.workspace_id.clone(),
                record.provider.clone(),
            ),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 8,
            selection_id: "hist_selection_1".to_owned(),
        };

        let requests = [
            NodeRequest::IndexProviderSession {
                workspace_id: record.workspace_id.clone(),
                provider: record.provider.clone(),
                identity: ProviderSessionIdentity {
                    key: gate4agent_types::ProviderSessionKey::SessionId,
                    id: "b1ef3250-47a2-42ca-9076-cc241487ea22".to_owned(),
                    transcript_path: None,
                },
                display_name: "release shepherd".to_owned(),
            },
            NodeRequest::IndexNativeSession {
                selection: native_selection.clone(),
                display_name: "release shepherd".to_owned(),
            },
            NodeRequest::RenameSessionRecord {
                record_id: record.record_id.clone(),
                display_name: "release verification".to_owned(),
            },
            NodeRequest::ResumeSessionRecord {
                record_id: record.record_id.clone(),
                terminal_size: TerminalSize { rows: 40, columns: 120 },
                initial_prompt: None,
            },
            NodeRequest::ForgetSessionRecord {
                record_id: record.record_id.clone(),
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
            if matches!(request, NodeRequest::IndexProviderSession { .. }) {
                assert_eq!(
                    request.required_capability(),
                    Some(NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY),
                );
            }
            if matches!(request, NodeRequest::IndexNativeSession { .. }) {
                assert_eq!(
                    request.required_capability(),
                    Some(NODE_NATIVE_SESSION_INDEX_CAPABILITY),
                );
            }
        }

        let native_indexed = NodeResponse::NativeSessionIndexed {
            selection: native_selection,
            record: record.clone(),
        };
        assert!(native_indexed.requires_native_session_index_capability());
        assert!(native_indexed.native_session_index_contract_is_valid());
        let responses = [
            NodeResponse::ProviderSessionIndexed {
                record: record.clone(),
            },
            native_indexed,
            NodeResponse::SessionRecordUpdated {
                record: record.clone(),
            },
            NodeResponse::SessionRecordResumed {
                record: record.clone(),
                session: SessionAddress {
                    workspace_id: record.workspace_id.clone(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(8),
                        generation: SessionGeneration(2),
                    },
                },
            },
            NodeResponse::SessionRecordForgotten {
                record_id: record.record_id.clone(),
            },
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<NodeResponse>(&json).unwrap(), response);
        }

        let events = [
            NodeEvent::SessionRecordUpserted {
                record: record.clone(),
            },
            NodeEvent::SessionRecordRemoved {
                record_id: record.record_id.clone(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<NodeEvent>(&json).unwrap(), event);
        }
    }

    #[test]
    fn session_record_ids_are_bounded_validated_wire_values() {
        let record_id = SessionRecordId::new("01j4k0jta3eynt5kxef132kr39").unwrap();
        assert_eq!(record_id.as_str(), "01j4k0jta3eynt5kxef132kr39");
        assert!(SessionRecordId::new("").is_err());
        assert!(SessionRecordId::new("Session-1").is_err());
        assert!(SessionRecordId::new("-session-1").is_err());
        assert!(SessionRecordId::new("x".repeat(MAX_NODE_IDENTIFIER_BYTES + 1)).is_err());

        let json = serde_json::to_string(&record_id).unwrap();
        assert_eq!(serde_json::from_str::<SessionRecordId>(&json).unwrap(), record_id);
        assert!(serde_json::from_str::<SessionRecordId>("\"Session-1\"").is_err());
    }

    #[test]
    fn node_snapshot_defaults_managed_sessions_for_legacy_wire_payloads() {
        let legacy = r#"{"node_id":"fixture-node","enabled_providers":[],"workspaces":[]}"#;
        let snapshot = serde_json::from_str::<NodeSnapshot>(legacy).unwrap();
        assert!(snapshot.session_records.is_empty());
    }

    fn delivery_manifest() -> DeliveryBundleManifestV2 {
        DeliveryBundleManifestV2 {
            bundle_id: SpawnBundleId::new("review-bundle").unwrap(),
            revision: SpawnBundleRevision::new("review-bundle.r2").unwrap(),
            bundle_digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
            manifest_digest: DeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "b".repeat(64),
            ))
            .unwrap(),
            components: vec![DeliveryComponentV2 {
                kind: DeliveryComponentKindV2::AgentDefinition,
                scope: DeliveryScopeV2::Workspace,
                relative_path: DeliveryRelativePathV2::new("agents/reviewer.md").unwrap(),
                blob: DeliveryBlobReceiptV1::new(
                    DeliveryBlobDigestV1::new(format!("sha256:{}", "c".repeat(64))).unwrap(),
                    12,
                )
                .unwrap(),
            }],
        }
    }

    #[test]
    fn delivery_wire_serialization_is_exact() {
        let begin = NodeRequest::BeginDeliveryStage {
            manifest: delivery_manifest(),
        };
        assert_eq!(
            serde_json::to_string(&begin).unwrap(),
            format!(
                r#"{{"kind":"begin-delivery-stage","manifest":{{"bundle_id":"review-bundle","revision":"review-bundle.r2","bundle_digest":"sha256:{}","manifest_digest":"sha256:{}","components":[{{"kind":"agent-definition","scope":"workspace","relative_path":"agents/reviewer.md","blob":{{"digest":"sha256:{}","byte_len":12}}}}]}}}}"#,
                "a".repeat(64),
                "b".repeat(64),
                "c".repeat(64),
            ),
        );

        let chunk = NodeRequest::PutDeliveryBlobChunk {
            stage_id: DeliveryStageId::new(format!(
                "delivery-stage-{}",
                "1".repeat(32),
            ))
            .unwrap(),
            blob_digest: DeliveryBlobDigestV1::new(format!("sha256:{}", "c".repeat(64)))
                .unwrap(),
            offset: 0,
            chunk_hex: DeliveryBlobChunkHexV1::new("00ff").unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&chunk).unwrap(),
            format!(
                r#"{{"kind":"put-delivery-blob-chunk","stage_id":"delivery-stage-{}","blob_digest":"sha256:{}","offset":0,"chunk_hex":"00ff"}}"#,
                "1".repeat(32),
                "c".repeat(64),
            ),
        );

        let committed = NodeResponse::DeliveryCommitted {
            receipt: DeliveryCommitReceiptV1 {
                bundle_id: SpawnBundleId::new("review-bundle").unwrap(),
                revision: SpawnBundleRevision::new("review-bundle.r2").unwrap(),
                bundle_digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64)))
                    .unwrap(),
                manifest_digest: DeliveryManifestDigestV2::new(format!(
                    "sha256:{}",
                    "b".repeat(64),
                ))
                .unwrap(),
                blobs: vec![DeliveryBlobReceiptV1::new(
                    DeliveryBlobDigestV1::new(format!("sha256:{}", "c".repeat(64))).unwrap(),
                    12,
                )
                .unwrap()],
            },
        };
        assert_eq!(
            serde_json::from_str::<NodeResponse>(&serde_json::to_string(&committed).unwrap())
                .unwrap(),
            committed,
        );
    }

    #[test]
    fn delivery_wire_rejects_bounds_order_and_case_fold_collisions() {
        assert!(DeliveryRelativePathV2::new("a".repeat(MAX_DELIVERY_RELATIVE_PATH_BYTES))
            .is_ok());
        assert!(DeliveryRelativePathV2::new("a".repeat(MAX_DELIVERY_RELATIVE_PATH_BYTES + 1))
            .is_err());
        for invalid in [
            "CON",
            "con.txt",
            "PRN.md",
            "AUX",
            "NUL.json",
            "COM1.txt",
            "LPT9",
            "trailing.",
            "trailing ",
            "bad<name",
            "bad>name",
            "bad\"name",
            "bad|name",
            "bad?name",
            "bad*name",
        ] {
            assert!(DeliveryRelativePathV2::new(invalid).is_err(), "{invalid}");
        }
        assert!(DeliveryBlobChunkHexV1::new("00".repeat(MAX_DELIVERY_CHUNK_RAW_BYTES)).is_ok());
        assert!(DeliveryBlobChunkHexV1::new("00".repeat(MAX_DELIVERY_CHUNK_RAW_BYTES + 1))
            .is_err());
        assert!(DeliveryBlobChunkHexV1::new("AA").is_err());
        assert!(DeliveryBlobChunkHexV1::new("").is_err());

        let mut manifest = delivery_manifest();
        let mut collision = manifest.components[0].clone();
        collision.relative_path = DeliveryRelativePathV2::new("AGENTS/REVIEWER.MD").unwrap();
        manifest.components.push(collision);
        manifest.components.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(serde_json::from_str::<DeliveryBundleManifestV2>(&json).is_err());

        let mut over_total = delivery_manifest();
        over_total.components.clear();
        for index in 0..=MAX_DELIVERY_TOTAL_BYTES / MAX_DELIVERY_FILE_BYTES {
            over_total.components.push(DeliveryComponentV2 {
                kind: DeliveryComponentKindV2::File,
                scope: DeliveryScopeV2::Workspace,
                relative_path: DeliveryRelativePathV2::new(format!("file-{index:03}.txt"))
                    .unwrap(),
                blob: DeliveryBlobReceiptV1::new(
                    DeliveryBlobDigestV1::new(format!(
                        "sha256:{index:064x}",
                    ))
                    .unwrap(),
                    MAX_DELIVERY_FILE_BYTES as u64,
                )
                .unwrap(),
            });
        }
        assert!(over_total.validate().is_err());
        assert!(DeliveryBlobReceiptV1::new(
            DeliveryBlobDigestV1::new(format!("sha256:{}", "d".repeat(64))).unwrap(),
            MAX_DELIVERY_FILE_BYTES as u64 + 1,
        )
        .is_err());

        let mut too_many = delivery_manifest();
        too_many.components = (0..=MAX_DELIVERY_FILES)
            .map(|index| DeliveryComponentV2 {
                kind: DeliveryComponentKindV2::File,
                scope: DeliveryScopeV2::Workspace,
                relative_path: DeliveryRelativePathV2::new(format!("file-{index:03}.txt"))
                    .unwrap(),
                blob: DeliveryBlobReceiptV1::new(
                    DeliveryBlobDigestV1::new(format!("sha256:{index:064x}")).unwrap(),
                    0,
                )
                .unwrap(),
            })
            .collect();
        assert!(serde_json::from_str::<DeliveryBundleManifestV2>(
            &serde_json::to_string(&too_many).unwrap(),
        )
        .is_err());

        let digest_a = format!("sha256:{}", "a".repeat(64));
        let digest_b = format!("sha256:{}", "b".repeat(64));
        let unsorted = format!(
            r#"{{"kind":"delivery-stage-begun","stage_id":"delivery-stage-{}","manifest_digest":"{}","missing_blobs":["{}","{}"]}}"#,
            "1".repeat(32),
            digest_a,
            digest_b,
            digest_a,
        );
        assert!(serde_json::from_str::<NodeResponse>(&unsorted).is_err());
    }
}
