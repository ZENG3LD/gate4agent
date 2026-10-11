//! Session materialization ownership records and path classes (transport-side shared types).

use crate::protocol::{
    ManagedWorktreeLeaseId, NodeIncarnationId, ResolvedBundleReceipt, ResolvedContextPackReceipt,
    ResolvedEnvironmentProfileReceipt, SessionRecordId,
};
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const MAX_SESSION_MATERIALIZATION_FILES: usize = 128;
pub const MAX_SESSION_MATERIALIZATION_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_SESSION_MATERIALIZATIONS: usize = 4_096;

pub fn path_to_opaque(path: &Path) -> io::Result<crate::protocol::OpaqueHostPath> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        crate::protocol::OpaqueHostPath::unix_bytes(path.as_os_str().as_bytes().to_vec()).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    #[cfg(windows)]
    {
        crate::protocol::OpaqueHostPath::utf8(path.to_string_lossy().into_owned()).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

pub fn opaque_to_path(path: &crate::protocol::OpaqueHostPath) -> io::Result<PathBuf> {
    if let Some(value) = path.as_utf8() { return Ok(PathBuf::from(value)); }
    #[cfg(unix)]
    if let Some(value) = path.as_unix_bytes() {
        use std::os::unix::ffi::OsStringExt;
        return Ok(PathBuf::from(OsString::from_vec(value.to_vec())));
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "materialization path encoding is unsupported"))
}

/// True when `path` is a non-empty, bounded, normal-components-only relative path.
pub fn relative_path_is_valid(path: &Path) -> bool {
    let Some(encoded) = path.to_str() else { return false };
    !(path.as_os_str().is_empty()
        || encoded.len() > MAX_SESSION_MATERIALIZATION_RELATIVE_PATH_BYTES
        || path.components().any(|component| !matches!(component, Component::Normal(_))))
}

pub fn valid_materialization_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn checked_timestamp(created: u64, now: u64) -> Result<u64, MaterializationRecordError> {
    if now < created { Err(MaterializationRecordError::InvalidRecord) } else { Ok(now) }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeSessionPathClass {
    ProviderHome,
    Config,
    Cache,
    Data,
    State,
    Tmp,
    #[doc(hidden)]
    BundleRoot,
    #[doc(hidden)]
    PluginData,
    #[doc(hidden)]
    Context,
}

impl NodeSessionPathClass {
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::ProviderHome => "home",
            Self::Config => "config",
            Self::Cache => "cache",
            Self::Data => "data",
            Self::State => "state",
            Self::Tmp => "tmp",
            Self::BundleRoot => "bundle",
            Self::PluginData => "plugin-data",
            Self::Context => "context",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MaterializationId(String);

impl MaterializationId {
    pub fn new(value: impl Into<String>) -> Result<Self, MaterializationRecordError> {
        let value = value.into();
        if !valid_materialization_id(&value) {
            return Err(MaterializationRecordError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for MaterializationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MaterializationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum MaterializationOwner {
    Session {
        incarnation_id: NodeIncarnationId,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    },
    Record {
        record_id: SessionRecordId,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationState {
    Preparing,
    Ready,
    CleanupRequired,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializedPathKind {
    Generated,
    Secret,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedPathDeclaration {
    pub class: NodeSessionPathClass,
    pub relative_path: PathBuf,
    pub kind: MaterializedPathKind,
}

#[derive(Clone, Eq, PartialEq)]
pub struct MaterializationOwnershipRecord {
    pub id: MaterializationId,
    pub environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    pub bundle: Option<ResolvedBundleReceipt>,
    pub context: Option<ResolvedContextPackReceipt>,
    pub owner: MaterializationOwner,
    pub managed_lease_id: Option<ManagedWorktreeLeaseId>,
    pub state: MaterializationState,
    pub root: PathBuf,
    pub provider_home: PathBuf,
    pub bundle_root: PathBuf,
    pub plugin_data: PathBuf,
    pub declared_paths: Vec<MaterializedPathDeclaration>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl MaterializationOwnershipRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted(
        id: MaterializationId,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
        owner: MaterializationOwner,
        managed_lease_id: Option<ManagedWorktreeLeaseId>,
        state: MaterializationState,
        root: PathBuf,
        provider_home: PathBuf,
        bundle_root: PathBuf,
        plugin_data: PathBuf,
        declared_paths: Vec<MaterializedPathDeclaration>,
        created_at_unix_ms: u64,
        updated_at_unix_ms: u64,
    ) -> Result<Self, MaterializationRecordError> {
        let record = Self {
            id,
            environment_profile,
            bundle,
            context,
            owner,
            managed_lease_id,
            state,
            root,
            provider_home,
            bundle_root,
            plugin_data,
            declared_paths,
            created_at_unix_ms,
            updated_at_unix_ms,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn id(&self) -> &MaterializationId { &self.id }
    pub fn environment_profile(&self) -> Option<&ResolvedEnvironmentProfileReceipt> { self.environment_profile.as_ref() }
    pub fn bundle(&self) -> Option<&ResolvedBundleReceipt> { self.bundle.as_ref() }
    pub fn context(&self) -> Option<&ResolvedContextPackReceipt> { self.context.as_ref() }
    pub fn owner(&self) -> &MaterializationOwner { &self.owner }
    pub fn managed_lease_id(&self) -> Option<&ManagedWorktreeLeaseId> { self.managed_lease_id.as_ref() }
    pub fn state(&self) -> MaterializationState { self.state }
    pub fn root(&self) -> &Path { &self.root }
    pub fn provider_home(&self) -> &Path { &self.provider_home }
    pub fn bundle_root(&self) -> &Path { &self.bundle_root }
    pub fn plugin_data(&self) -> &Path { &self.plugin_data }
    pub fn declared_paths(&self) -> &[MaterializedPathDeclaration] { &self.declared_paths }
    pub fn created_at_unix_ms(&self) -> u64 { self.created_at_unix_ms }
    pub fn updated_at_unix_ms(&self) -> u64 { self.updated_at_unix_ms }

    pub fn mark_ready(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
        self.transition(MaterializationState::Preparing, MaterializationState::Ready, now)
    }

    pub fn transfer_to_record(
        &mut self,
        record_id: SessionRecordId,
        now: u64,
    ) -> Result<(), MaterializationRecordError> {
        if self.state != MaterializationState::Ready || !matches!(self.owner, MaterializationOwner::Session { .. }) {
            return Err(MaterializationRecordError::InvalidTransition);
        }
        self.owner = MaterializationOwner::Record { record_id };
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    pub fn transfer_record_owner(
        &mut self,
        expected_record_id: &SessionRecordId,
        replacement_record_id: SessionRecordId,
        now: u64,
    ) -> Result<(), MaterializationRecordError> {
        if self.state != MaterializationState::Ready
            || !matches!(
                &self.owner,
                MaterializationOwner::Record { record_id }
                    if record_id == expected_record_id
            )
        {
            return Err(MaterializationRecordError::InvalidTransition);
        }
        self.owner = MaterializationOwner::Record {
            record_id: replacement_record_id,
        };
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    pub fn mark_cleanup_required(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
        if !matches!(self.state, MaterializationState::Preparing | MaterializationState::Ready | MaterializationState::RecoveryRequired) {
            return Err(MaterializationRecordError::InvalidTransition);
        }
        self.state = MaterializationState::CleanupRequired;
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    pub fn mark_recovery_required(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
        if self.state == MaterializationState::RecoveryRequired {
            return Ok(());
        }
        self.state = MaterializationState::RecoveryRequired;
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    fn transition(&mut self, from: MaterializationState, to: MaterializationState, now: u64) -> Result<(), MaterializationRecordError> {
        if self.state != from { return Err(MaterializationRecordError::InvalidTransition); }
        self.state = to;
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), MaterializationRecordError> {
        if (self.environment_profile.is_none() && self.bundle.is_none() && self.context.is_none())
            || !self.root.is_absolute()
            || !self.provider_home.is_absolute()
            || !self.bundle_root.is_absolute()
            || !self.plugin_data.is_absolute()
            || self.provider_home != self.root.join(NodeSessionPathClass::ProviderHome.directory_name())
            || self.bundle_root != self.root.join(NodeSessionPathClass::BundleRoot.directory_name())
            || self.plugin_data != self.root.join(NodeSessionPathClass::PluginData.directory_name())
            || self.created_at_unix_ms > self.updated_at_unix_ms
            || self.declared_paths.len() > MAX_SESSION_MATERIALIZATION_FILES
        {
            return Err(MaterializationRecordError::InvalidRecord);
        }
        let mut seen = BTreeSet::new();
        for (index, declaration) in self.declared_paths.iter().enumerate() {
            if !relative_path_is_valid(&declaration.relative_path) { return Err(MaterializationRecordError::InvalidRecord); }
            if !seen.insert((declaration.class, declaration.relative_path.clone())) {
                return Err(MaterializationRecordError::InvalidRecord);
            }
            for other in self.declared_paths.iter().skip(index + 1) {
                if declaration.class == other.class
                    && (declaration.relative_path.starts_with(&other.relative_path)
                        || other.relative_path.starts_with(&declaration.relative_path))
                {
                    return Err(MaterializationRecordError::InvalidRecord);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MaterializationRecordError {
    #[error("materialization ID is invalid")]
    InvalidId,
    #[error("materialization ownership record is invalid")]
    InvalidRecord,
    #[error("materialization state transition is invalid")]
    InvalidTransition,
}
