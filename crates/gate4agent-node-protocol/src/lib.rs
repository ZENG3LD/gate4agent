//! Bounded wire contract for the local Gate4Agent node.

pub use gate4agent_types::{AdapterFamily, AdapterId, AgentId};
use gate4agent_types::{
    AgentInstanceId, ControlEvent, ProviderSessionIdentity, SessionGeneration, SessionSnapshot,
    TerminalControl, TerminalFrame, TerminalSize,
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

pub const NODE_PROTOCOL_VERSION: u16 = 8;
pub const NODE_STATE_SCHEMA_V1: u16 = 1;
pub const NODE_STATE_SCHEMA_V2: u16 = 2;
pub const NODE_STATE_SCHEMA_V3: u16 = 3;
pub const NODE_STATE_SCHEMA_V4: u16 = 4;
pub const NODE_STATE_SCHEMA_V5: u16 = 5;
pub const NODE_STATE_SCHEMA_V6: u16 = 6;
pub const NODE_STATE_SCHEMA_V7: u16 = 7;
pub const NODE_COMPATIBILITY_METADATA_CAPABILITY: &str = "compatibility.metadata";
pub const NODE_OPAQUE_UNIX_PATH_CAPABILITY: &str = "path.opaque-unix-bytes-v1";
pub const NODE_REPOSITORY_PATH_CAPABILITY: &str = "repository-path-v1";
pub const NODE_WORKSPACE_FILE_READ_CAPABILITY: &str = "workspace-file-read-v1";
pub const NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY: &str = "provider-contract-manifest-v1";
pub const NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY: &str = "provider-runtime-status-v1";
pub const NODE_PROVIDER_ID_OPEN_CAPABILITY: &str = "provider-id.open-v1";
pub const NODE_TERMINAL_FRAME_EVENTS_CAPABILITY: &str = "terminal-frame-events-v1";
pub const NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY: &str =
    "spawn-spec.defaults-overrides-v1";
pub const NODE_WORKTREE_SELECTION_CAPABILITY: &str = "worktree-selection-v1";
pub const NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY: &str =
    "managed-worktree-lifecycle-v1";
pub const NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY: &str =
    "child-environment-profile-v1";
pub const NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY: &str =
    "session-bundle-materialization-v1";
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
pub const NODE_INCARNATION_ID_BYTES: usize = 16;
pub const MAX_NODE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODE_CLIENT_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_NODE_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_NODE_TERMINAL_BYTES: usize = 64;
pub const MAX_SESSION_DISPLAY_NAME_BYTES: usize = 256;
pub const MAX_SPAWN_PROFILE_ID_BYTES: usize = 64;
pub const MAX_SPAWN_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_ENVIRONMENT_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_BUNDLE_REVISION_BYTES: usize = 128;
pub const MAX_SPAWN_RESOURCE_ID_BYTES: usize = 128;
pub const MAX_SPAWN_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub const MAX_SPAWN_REQUIRED_CAPABILITIES: usize = 16;
pub const MAX_WORKTREE_PROFILE_ID_BYTES: usize = 64;
pub const MAX_WORKTREE_PROFILE_REVISION_BYTES: usize = 128;
pub const MAX_MANAGED_WORKTREE_LEASE_ID_BYTES: usize = 128;
pub const MAX_MANAGED_WORKTREE_LEASES: usize = 128;
pub const MAX_SPAWN_DEADLINE_MS: u64 = 120_000;
pub const MAX_WORKSPACE_ROOT_BYTES: usize = gate4agent_types::WORKING_DIRECTORY_MAX_BYTES;
pub const MAX_REPOSITORY_PATH_BYTES: usize = 1_024;
pub const MAX_WORKSPACE_FILE_BYTES: usize = 256 * 1024;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnTarget {
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    #[serde(rename = "environment_profile_id")]
    pub environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    pub deadline_ms: SpawnDeadlineMs,
    pub idempotency_key: SpawnIdempotencyKey,
    pub required_capabilities: SpawnRequiredCapabilities,
    pub provenance: SpawnResolutionProvenance,
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
            profile_revision: defaults.revision.clone(),
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
        self.receipt_with_materialization(incarnation_id, session, None, None)
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
        )
    }

    pub fn receipt_with_materialization(
        &self,
        incarnation_id: NodeIncarnationId,
        session: SessionAddress,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
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
            environment_profile,
            deadline_ms: self.deadline_ms,
            idempotency_key: self.idempotency_key.clone(),
            required_capabilities: self.required_capabilities.clone(),
            provenance: self.provenance.clone(),
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
            NODE_OPAQUE_UNIX_PATH_CAPABILITY,
            NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY,
            NODE_PROVIDER_ID_OPEN_CAPABILITY,
            NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY,
            NODE_REPOSITORY_PATH_CAPABILITY,
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
            NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
            NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
            NODE_WORKSPACE_FILE_READ_CAPABILITY,
            NODE_WORKTREE_SELECTION_CAPABILITY,
        ]
        .into_iter()
        .map(|capability| CapabilityId(capability.to_owned()))
        .collect(),
        state_schema: Some(StateSchemaSupport {
            versions: ProtocolRange {
                minimum: NODE_STATE_SCHEMA_V1,
                maximum: NODE_STATE_SCHEMA_V7,
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
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub sessions: Vec<SessionSnapshot>,
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
pub struct GitSnapshot {
    pub is_repository: bool,
    pub branch: Option<String>,
    pub status: Vec<GitStatusEntry>,
    pub recent_commits: Vec<GitCommitSummary>,
    #[serde(default)]
    pub worktrees: Vec<GitWorktreeSnapshot>,
    pub truncated: bool,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceInspection {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<WorkspaceEntry>,
    pub tree_truncated: bool,
    pub git: GitSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceFileRead {
    pub workspace_id: WorkspaceId,
    pub path: RepositoryPath,
    pub content: WorkspaceFileContent,
}

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
    InspectWorkspace { workspace_id: WorkspaceId },
    ReadWorkspaceFile {
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    },
    AcquireController { lease_ms: u64 },
    ReleaseController,
    RegisterWorkspace {
        workspace_id: WorkspaceId,
        root: OpaqueHostPath,
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
    ResumeSessionRecord {
        record_id: SessionRecordId,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    ForgetSessionRecord {
        record_id: SessionRecordId,
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

impl NodeRequest {
    pub fn required_capability(&self) -> Option<&'static str> {
        match self {
            Self::ReadWorkspaceFile { .. } => Some(NODE_WORKSPACE_FILE_READ_CAPABILITY),
            Self::SpawnSpec { .. } => Some(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY),
            Self::SpawnManagedWorktree { .. }
            | Self::CleanupManagedWorktree { .. } => {
                Some(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY)
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
            || matches!(
                self,
                Self::SpawnManagedWorktree { .. } | Self::CleanupManagedWorktree { .. }
            )
    }

    pub fn requires_spawn_spec_defaults_overrides_capability(&self) -> bool {
        matches!(
            self,
            Self::SpawnSpec { .. } | Self::SpawnManagedWorktree { .. }
        )
    }

    pub fn requires_child_environment_profile_capability(&self) -> bool {
        let spec = match self {
            Self::SpawnSpec { spec } => spec,
            Self::SpawnManagedWorktree { request } => &request.spawn_spec,
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
            Self::SpawnManagedWorktree { request } => &request.spawn_spec,
            _ => return false,
        };
        !matches!(spec.overrides.bundle_id, SpawnOverride::Clear)
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
        snapshot: NodeSnapshot,
        events: Vec<NodeEventEnvelope>,
    },
    WorkspaceInspected {
        inspection: WorkspaceInspection,
    },
    WorkspaceFileRead {
        file: WorkspaceFileRead,
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
    SessionRecordResumed {
        record: ManagedSessionRecord,
        session: SessionAddress,
    },
    SessionRecordForgotten {
        record_id: SessionRecordId,
    },
    WorkspaceRegistered {
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
    pub fn requires_worktree_selection_capability(&self) -> bool {
        matches!(self, Self::SpawnSpecAccepted { receipt }
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
            Self::SpawnSpecAccepted { .. } | Self::ManagedWorktreeSpawnAccepted { .. }
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
            Self::SpawnSpecAccepted { receipt } => receipt.environment_profile.is_some(),
            Self::ManagedWorktreeSpawnAccepted { receipt } => {
                receipt.spawn.environment_profile.is_some()
            }
            Self::SessionRecordUpdated { record }
            | Self::SessionRecordResumed { record, .. } => {
                record.environment_profile.is_some()
            }
            Self::WorkspaceInspected { .. }
            | Self::WorkspaceFileRead { .. }
            | Self::Controller { .. }
            | Self::SpawnAccepted { .. }
            | Self::ManagedWorktreeCleanup { .. }
            | Self::SessionRecordForgotten { .. }
            | Self::WorkspaceRegistered { .. }
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
            Self::SpawnSpecAccepted { receipt } => receipt.bundle.is_some(),
            Self::ManagedWorktreeSpawnAccepted { receipt } => receipt.spawn.bundle.is_some(),
            Self::SessionRecordUpdated { record }
            | Self::SessionRecordResumed { record, .. } => record.bundle.is_some(),
            Self::WorkspaceInspected { .. }
            | Self::WorkspaceFileRead { .. }
            | Self::Controller { .. }
            | Self::SpawnAccepted { .. }
            | Self::ManagedWorktreeCleanup { .. }
            | Self::SessionRecordForgotten { .. }
            | Self::WorkspaceRegistered { .. }
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
    Unauthorized,
    ObserverReadOnly,
    ControllerBusy,
    ControllerRequired,
    UnknownWorkspace,
    InvalidRepositoryPath,
    RepositoryFileNotFound,
    RepositoryFileNotRegular,
    RepositoryPathUnsafe,
    RepositoryFileReadTimedOut,
    RepositoryFileReadFailed,
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
    ManagedWorktreeRecoveryRequired,
    UnknownSpawnProfile,
    UnknownBundle,
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
    Control { address: SessionAddress, event: ControlEvent },
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
    pub fn requires_child_environment_profile_capability(&self) -> bool {
        matches!(self, Self::SessionRecordUpserted { record }
            if record.environment_profile.is_some())
    }


    pub fn requires_session_bundle_materialization_capability(&self) -> bool {
        matches!(self, Self::SessionRecordUpserted { record }
            if record.bundle.is_some())
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

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn host_path(value: &str) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.to_owned()).unwrap()
    }

    fn repository_path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
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
    fn legacy_hello_json_remains_exactly_protocol_v8() {
        let client = ClientHello::new(ClientRole::Observer, [0; NODE_AUTH_NONCE_BYTES]);
        assert_eq!(
            serde_json::to_string(&client).unwrap(),
            r#"{"protocol_version":8,"role":"observer","client_nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#,
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

        let receipt = first.receipt(
            NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            SessionAddress {
                workspace_id: WorkspaceId::new("review-tree").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(11),
                    generation: SessionGeneration(1),
                },
            },
        );
        assert_eq!(receipt.prompt, SpawnPromptMetadata {
            present: true,
            byte_len: 19,
        });
        let receipt_json = serde_json::to_string(&receipt).unwrap();
        assert_eq!(
            receipt_json,
            r#"{"incarnation_id":"09090909090909090909090909090909","session":{"workspace_id":"review-tree","session":{"instance_id":11,"generation":1}},"target":{"node_id":"node-a","workspace_id":"primary","worktree_id":"review-tree"},"profile_id":"review-default","profile_revision":"review-default.r3","provider":"claude","mode":"inline","terminal_size":{"rows":31,"columns":97},"prompt":{"present":true,"byte_len":19},"bundle_id":null,"context_id":"repo-context","environment_profile_id":null,"deadline_ms":30000,"idempotency_key":"request-0001","required_capabilities":["raw-pty-lifecycle","structured-prompt"],"provenance":{"provider":"profile","mode":"override","terminal_size":"override","prompt":"override","bundle_id":"cleared","context_id":"profile","environment_profile_id":"cleared"}}"#,
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
        let environment_receipt = first.receipt_with_environment(
            NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            receipt.session.clone(),
            Some(environment_profile.clone()),
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

        let minimal_json = r#"{"target":{"node_id":"node-a","workspace_id":"primary"},"profile_id":"review-default","deadline_ms":1,"idempotency_key":"request-0002"}"#;
        let minimal = serde_json::from_str::<SpawnSpec>(minimal_json).unwrap();
        assert_eq!(minimal.overrides, SpawnOverrides::default());
        assert!(minimal.required_capabilities.is_empty());
        assert!(SpawnDeadlineMs::new(MAX_SPAWN_DEADLINE_MS + 1).is_err());
        assert!(SpawnProfileId::new("unsafe/profile").is_err());
        assert!(serde_json::from_str::<SpawnOverride<AgentId>>(
            r#"{"kind":"set","value":"claude","typo":true}"#,
        )
        .is_err());
    }

    #[test]
    fn session_bundle_materialization_contract_is_bounded_exact_and_dual_gated() {
        assert_eq!(NODE_PROTOCOL_VERSION, 8);
        assert_eq!(NODE_STATE_SCHEMA_V7, 7);
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

        let minimal_json = r#"{"target":{"node_id":"node-a","workspace_id":"primary"},"profile_id":"review-default","deadline_ms":1,"idempotency_key":"request-bundle"}"#;
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
            protocol_versions: ProtocolRange::new(9, 10).unwrap(),
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
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":10},"capabilities":["session.spawn"],"state_schema":{"versions":{"minimum":4,"maximum":6}}},"selected":{"protocol_version":8,"capabilities":["session.spawn"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","state_schema_version":5,"provider_contracts":[]}}"#,
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
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":8},"capabilities":["provider-contract-manifest-v1"]},"selected":{"protocol_version":8,"capabilities":["provider-contract-manifest-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[{"provider":"codex","revision":"codex.2026-08"}],"provider_adapter_contracts":[{"provider":"codex","family":"pty-semantic","adapter_id":"codex-cli","revision":"pty-semantic-v1"}]}}"#,
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
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":8},"capabilities":["provider-contract-manifest-v1","provider-id.open-v1"]},"selected":{"protocol_version":8,"capabilities":["provider-contract-manifest-v1","provider-id.open-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[{"provider":"qwen","revision":"qwen.2026-08"}]}}"#,
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
    fn protocol_v8_workspace_and_worktree_mutations_have_exact_bounded_wire_shapes() {
        assert_eq!(NODE_PROTOCOL_VERSION, 8);
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
            },
            compatibility: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":8,"incarnation_id":"00000000000000000000000000000000","connection_id":42,"role":"observer","event_sequence":9,"controller":null,"snapshot":{"node_id":"fixture-node","enabled_providers":[],"workspaces":[],"session_records":[]}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeHello>(&json).unwrap(), hello);
    }

    #[test]
    fn managed_worktree_contract_is_bounded_dual_gated_and_path_free() {
        assert_eq!(
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
            "managed-worktree-lifecycle-v1",
        );
        assert_eq!(NODE_PROTOCOL_VERSION, 8);
        assert_eq!(NODE_STATE_SCHEMA_V4, 4);
        assert_eq!(NODE_STATE_SCHEMA_V5, 5);
        assert_eq!(NODE_STATE_SCHEMA_V6, 6);
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
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":8},"capabilities":["terminal-frame-events-v1"]},"selected":{"protocol_version":8,"capabilities":["terminal-frame-events-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[]}}"#,
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
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":8},"capabilities":["worktree-selection-v1"]},"selected":{"protocol_version":8,"capabilities":["worktree-selection-v1"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","provider_contracts":[]}}"#,
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
        };
        let registered = NodeResponse::WorkspaceRegistered {
            workspace: workspace.clone(),
        };
        let registered_json = serde_json::to_string(&registered).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeResponse>(&registered_json).unwrap(),
            registered,
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
                    truncated: false,
                    diagnostic: None,
                },
            },
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&response_json).unwrap(), response);
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
    fn workspace_file_read_worst_case_json_stays_within_the_server_frame_limit() {
        let response = NodeResponse::WorkspaceFileRead {
            file: WorkspaceFileRead {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                path: repository_path(&"x".repeat(MAX_REPOSITORY_PATH_BYTES)),
                content: WorkspaceFileContent::Utf8 {
                    text: "\0".repeat(MAX_WORKSPACE_FILE_BYTES),
                    byte_len: u32::try_from(MAX_WORKSPACE_FILE_BYTES).unwrap(),
                },
            },
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= MAX_NODE_FRAME_BYTES, "{}", encoded.len());
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
            created_at_unix_ms: 1_723_000_000_000,
            updated_at_unix_ms: 1_723_000_000_123,
            last_error: None,
        };
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("environment_profile"));

        let requests = [
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
        }

        let responses = [
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
}
