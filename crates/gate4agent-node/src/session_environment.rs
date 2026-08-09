use crate::protocol::{
    ManagedWorktreeLeaseId, NodeIncarnationId, OpaqueHostPath,
    ResolvedEnvironmentProfileReceipt, SessionRecordId,
};
use gate4agent_catalog::EnvMutation;
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

pub const MAX_NODE_SECRET_REFERENCE_BYTES: usize = 256;
pub const MAX_NODE_SECRET_VALUE_BYTES: usize = 256 * 1024;
pub const MAX_SESSION_ENVIRONMENT_ENTRIES: usize = 128;
pub const MAX_SESSION_MATERIALIZATION_FILES: usize = 128;
pub const MAX_SESSION_MATERIALIZATION_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_SESSION_MATERIALIZATION_RELATIVE_PATH_BYTES: usize = 512;
pub(crate) const MAX_SESSION_MATERIALIZATIONS: usize = 4_096;

const MATERIALIZATION_ROOT_MARKER: &[u8] = b"gate4agent-node-session-environment-v1\n";
const MATERIALIZATION_OWNER_MARKER: &str = ".gate4agent-materialization-owner";
const MATERIALIZATION_ROOT_MARKER_NAME: &str = ".gate4agent-materialization-root";
const MATERIALIZATION_LOCK_NAME: &str = ".gate4agent-materialization-lock";

#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeSecretReference(String);

impl NodeSecretReference {
    pub fn new(value: impl Into<String>) -> Result<Self, NodeSessionMaterializationProfileError> {
        let value = value.into();
        if !valid_opaque_identifier(&value, MAX_NODE_SECRET_REFERENCE_BYTES) {
            return Err(NodeSessionMaterializationProfileError::InvalidSecretReference);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub enum NodeSecretValue {
    Text(String),
    Bytes(Vec<u8>),
}

impl NodeSecretValue {
    pub fn text(value: impl Into<String>) -> Result<Self, NodeSecretValueError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_NODE_SECRET_VALUE_BYTES || value.contains('\0') {
            return Err(NodeSecretValueError::Invalid);
        }
        Ok(Self::Text(value))
    }

    pub fn bytes(value: Vec<u8>) -> Result<Self, NodeSecretValueError> {
        if value.is_empty() || value.len() > MAX_NODE_SECRET_VALUE_BYTES {
            return Err(NodeSecretValueError::Invalid);
        }
        Ok(Self::Bytes(value))
    }

    fn into_environment_value(self) -> Result<OsString, SessionEnvironmentMaterializeError> {
        match self {
            Self::Text(value) => Ok(OsString::from(value)),
            Self::Bytes(value) => String::from_utf8(value)
                .map(OsString::from)
                .map_err(|_| SessionEnvironmentMaterializeError::InvalidSecretValue),
        }
    }

    fn into_file_bytes(self) -> Vec<u8> {
        match self {
            Self::Text(value) => value.into_bytes(),
            Self::Bytes(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NodeSecretValueError {
    #[error("secret value is outside the bounded value contract")]
    Invalid,
}

pub trait NodeSecretResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        reference: &NodeSecretReference,
    ) -> Result<NodeSecretValue, NodeSecretResolveError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NodeSecretResolveError {
    #[error("secret is unavailable")]
    Unavailable,
    #[error("secret access is denied")]
    Denied,
}

pub enum NodeSessionEnvironmentMutation {
    SetNonSecret { key: String, value: String },
    SetSecret { key: String, reference: NodeSecretReference },
    Remove { key: String },
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
}

impl NodeSessionPathClass {
    fn directory_name(self) -> &'static str {
        match self {
            Self::ProviderHome => "home",
            Self::Config => "config",
            Self::Cache => "cache",
            Self::Data => "data",
            Self::State => "state",
            Self::Tmp => "tmp",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeSessionPathBinding {
    key: String,
    class: NodeSessionPathClass,
}

impl NodeSessionPathBinding {
    pub fn new(
        key: impl Into<String>,
        class: NodeSessionPathClass,
    ) -> Result<Self, NodeSessionMaterializationProfileError> {
        let key = key.into();
        validate_environment_key(&key)?;
        Ok(Self { key, class })
    }
}

pub enum NodeSessionFile {
    Generated {
        class: NodeSessionPathClass,
        relative_path: PathBuf,
        contents: Vec<u8>,
    },
    Secret {
        class: NodeSessionPathClass,
        relative_path: PathBuf,
        reference: NodeSecretReference,
    },
}

impl NodeSessionFile {
    pub fn generated(
        class: NodeSessionPathClass,
        relative_path: impl Into<PathBuf>,
        contents: Vec<u8>,
    ) -> Result<Self, NodeSessionMaterializationProfileError> {
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        if contents.len() > MAX_SESSION_MATERIALIZATION_FILE_BYTES {
            return Err(NodeSessionMaterializationProfileError::FileTooLarge);
        }
        Ok(Self::Generated { class, relative_path, contents })
    }

    pub fn secret(
        class: NodeSessionPathClass,
        relative_path: impl Into<PathBuf>,
        reference: NodeSecretReference,
    ) -> Result<Self, NodeSessionMaterializationProfileError> {
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        Ok(Self::Secret { class, relative_path, reference })
    }

    fn declaration(&self) -> MaterializedPathDeclaration {
        match self {
            Self::Generated { class, relative_path, .. } => MaterializedPathDeclaration {
                class: *class,
                relative_path: relative_path.clone(),
                kind: MaterializedPathKind::Generated,
            },
            Self::Secret { class, relative_path, .. } => MaterializedPathDeclaration {
                class: *class,
                relative_path: relative_path.clone(),
                kind: MaterializedPathKind::Secret,
            },
        }
    }
}

struct NodeSessionMaterializationProfileInner {
    environment: Vec<NodeSessionEnvironmentMutation>,
    path_bindings: Vec<NodeSessionPathBinding>,
    files: Vec<NodeSessionFile>,
}

#[derive(Clone)]
pub struct NodeSessionMaterializationProfile(Arc<NodeSessionMaterializationProfileInner>);

impl NodeSessionMaterializationProfile {
    pub fn new(
        environment: Vec<NodeSessionEnvironmentMutation>,
        path_bindings: Vec<NodeSessionPathBinding>,
        files: Vec<NodeSessionFile>,
    ) -> Result<Self, NodeSessionMaterializationProfileError> {
        if environment.len() + path_bindings.len() > MAX_SESSION_ENVIRONMENT_ENTRIES {
            return Err(NodeSessionMaterializationProfileError::TooManyEnvironmentEntries);
        }
        if files.len() > MAX_SESSION_MATERIALIZATION_FILES {
            return Err(NodeSessionMaterializationProfileError::TooManyFiles);
        }
        let mut environment_keys = BTreeSet::new();
        for mutation in &environment {
            let key = match mutation {
                NodeSessionEnvironmentMutation::SetNonSecret { key, value } => {
                    if value.len() > MAX_NODE_SECRET_VALUE_BYTES || value.contains('\0') {
                        return Err(NodeSessionMaterializationProfileError::InvalidEnvironmentValue);
                    }
                    key
                }
                NodeSessionEnvironmentMutation::SetSecret { key, .. }
                | NodeSessionEnvironmentMutation::Remove { key } => key,
            };
            validate_environment_key(key)?;
            if !environment_keys.insert(normalized_environment_key(key)) {
                return Err(NodeSessionMaterializationProfileError::DuplicateEnvironmentKey);
            }
        }
        for binding in &path_bindings {
            if !environment_keys.insert(normalized_environment_key(&binding.key)) {
                return Err(NodeSessionMaterializationProfileError::DuplicateEnvironmentKey);
            }
        }
        let declarations = files.iter().map(NodeSessionFile::declaration).collect::<Vec<_>>();
        for (index, left) in declarations.iter().enumerate() {
            for right in declarations.iter().skip(index + 1) {
                if left.class == right.class
                    && (left.relative_path == right.relative_path
                        || left.relative_path.starts_with(&right.relative_path)
                        || right.relative_path.starts_with(&left.relative_path))
                {
                    return Err(NodeSessionMaterializationProfileError::ConflictingFilePath);
                }
            }
        }
        Ok(Self(Arc::new(NodeSessionMaterializationProfileInner {
            environment,
            path_bindings,
            files,
        })))
    }

    pub fn is_empty(&self) -> bool {
        self.0.environment.is_empty() && self.0.path_bindings.is_empty() && self.0.files.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NodeSessionMaterializationProfileError {
    #[error("secret reference is outside the bounded opaque identifier contract")]
    InvalidSecretReference,
    #[error("environment key is invalid")]
    InvalidEnvironmentKey,
    #[error("environment value is invalid")]
    InvalidEnvironmentValue,
    #[error("materialization profile contains duplicate normalized environment keys")]
    DuplicateEnvironmentKey,
    #[error("materialization profile contains too many environment entries")]
    TooManyEnvironmentEntries,
    #[error("materialization profile contains too many files")]
    TooManyFiles,
    #[error("materialization file exceeds the bounded size")]
    FileTooLarge,
    #[error("materialization relative path is invalid")]
    InvalidRelativePath,
    #[error("materialization file paths overlap")]
    ConflictingFilePath,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MaterializationId(String);

impl MaterializationId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, MaterializationRecordError> {
        let value = value.into();
        if !valid_materialization_id(&value) {
            return Err(MaterializationRecordError::InvalidId);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
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
pub(crate) enum MaterializationOwner {
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
pub(crate) enum MaterializationState {
    Preparing,
    Ready,
    CleanupRequired,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MaterializedPathKind {
    Generated,
    Secret,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedPathDeclaration {
    pub(crate) class: NodeSessionPathClass,
    pub(crate) relative_path: PathBuf,
    pub(crate) kind: MaterializedPathKind,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct MaterializationOwnershipRecord {
    id: MaterializationId,
    environment_profile: ResolvedEnvironmentProfileReceipt,
    owner: MaterializationOwner,
    managed_lease_id: Option<ManagedWorktreeLeaseId>,
    state: MaterializationState,
    root: PathBuf,
    provider_home: PathBuf,
    declared_paths: Vec<MaterializedPathDeclaration>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
}

impl MaterializationOwnershipRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_persisted(
        id: MaterializationId,
        environment_profile: ResolvedEnvironmentProfileReceipt,
        owner: MaterializationOwner,
        managed_lease_id: Option<ManagedWorktreeLeaseId>,
        state: MaterializationState,
        root: PathBuf,
        provider_home: PathBuf,
        declared_paths: Vec<MaterializedPathDeclaration>,
        created_at_unix_ms: u64,
        updated_at_unix_ms: u64,
    ) -> Result<Self, MaterializationRecordError> {
        let record = Self {
            id,
            environment_profile,
            owner,
            managed_lease_id,
            state,
            root,
            provider_home,
            declared_paths,
            created_at_unix_ms,
            updated_at_unix_ms,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn id(&self) -> &MaterializationId { &self.id }
    pub(crate) fn environment_profile(&self) -> &ResolvedEnvironmentProfileReceipt { &self.environment_profile }
    pub(crate) fn owner(&self) -> &MaterializationOwner { &self.owner }
    pub(crate) fn managed_lease_id(&self) -> Option<&ManagedWorktreeLeaseId> { self.managed_lease_id.as_ref() }
    pub(crate) fn state(&self) -> MaterializationState { self.state }
    pub(crate) fn root(&self) -> &Path { &self.root }
    pub(crate) fn provider_home(&self) -> &Path { &self.provider_home }
    pub(crate) fn declared_paths(&self) -> &[MaterializedPathDeclaration] { &self.declared_paths }
    pub(crate) fn created_at_unix_ms(&self) -> u64 { self.created_at_unix_ms }
    pub(crate) fn updated_at_unix_ms(&self) -> u64 { self.updated_at_unix_ms }

    pub(crate) fn mark_ready(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
        self.transition(MaterializationState::Preparing, MaterializationState::Ready, now)
    }

    pub(crate) fn transfer_to_record(
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

    pub(crate) fn transfer_record_owner(
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

    pub(crate) fn mark_cleanup_required(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
        if !matches!(self.state, MaterializationState::Preparing | MaterializationState::Ready | MaterializationState::RecoveryRequired) {
            return Err(MaterializationRecordError::InvalidTransition);
        }
        self.state = MaterializationState::CleanupRequired;
        self.updated_at_unix_ms = checked_timestamp(self.created_at_unix_ms, now)?;
        Ok(())
    }

    pub(crate) fn mark_recovery_required(&mut self, now: u64) -> Result<(), MaterializationRecordError> {
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

    pub(crate) fn validate(&self) -> Result<(), MaterializationRecordError> {
        if !self.root.is_absolute()
            || !self.provider_home.is_absolute()
            || self.provider_home != self.root.join(NodeSessionPathClass::ProviderHome.directory_name())
            || self.created_at_unix_ms > self.updated_at_unix_ms
            || self.declared_paths.len() > MAX_SESSION_MATERIALIZATION_FILES
        {
            return Err(MaterializationRecordError::InvalidRecord);
        }
        let mut seen = BTreeSet::new();
        for (index, declaration) in self.declared_paths.iter().enumerate() {
            validate_relative_path(&declaration.relative_path).map_err(|_| MaterializationRecordError::InvalidRecord)?;
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
pub(crate) enum MaterializationRecordError {
    #[error("materialization ID is invalid")]
    InvalidId,
    #[error("materialization ownership record is invalid")]
    InvalidRecord,
    #[error("materialization state transition is invalid")]
    InvalidTransition,
}

#[cfg(test)]
struct PreparedSessionEnvironment {
    environment: Vec<EnvMutation>,
    ownership: MaterializationOwnershipRecord,
}

#[cfg(test)]
impl PreparedSessionEnvironment {
    fn environment(&self) -> &[EnvMutation] { &self.environment }
    fn into_parts(self) -> (Vec<EnvMutation>, MaterializationOwnershipRecord) {
        (self.environment, self.ownership)
    }
}

pub(crate) struct SessionEnvironmentMaterializer {
    root: PathBuf,
    resolver: Arc<dyn NodeSecretResolver>,
    _lock: MaterializationRootLock,
}

impl SessionEnvironmentMaterializer {
    pub(crate) fn new(
        root: PathBuf,
        resolver: Arc<dyn NodeSecretResolver>,
    ) -> Result<Self, SessionEnvironmentMaterializeError> {
        if !root.is_absolute() {
            return Err(SessionEnvironmentMaterializeError::InvalidRoot);
        }
        ensure_materialization_root(&root)?;
        let lock = MaterializationRootLock::acquire(&root.join(MATERIALIZATION_LOCK_NAME))?;
        verify_or_create_exact_file(&root.join(MATERIALIZATION_ROOT_MARKER_NAME), MATERIALIZATION_ROOT_MARKER)?;
        Ok(Self { root, resolver, _lock: lock })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin(
        &self,
        id: MaterializationId,
        environment_profile: ResolvedEnvironmentProfileReceipt,
        owner: MaterializationOwner,
        managed_lease_id: Option<ManagedWorktreeLeaseId>,
        profile: &NodeSessionMaterializationProfile,
        now: u64,
    ) -> Result<MaterializationOwnershipRecord, SessionEnvironmentMaterializeError> {
        let materialization_root = self.root.join(id.as_str());
        MaterializationOwnershipRecord::from_persisted(
            id,
            environment_profile,
            owner,
            managed_lease_id,
            MaterializationState::Preparing,
            materialization_root.clone(),
            materialization_root.join(NodeSessionPathClass::ProviderHome.directory_name()),
            profile.0.files.iter().map(NodeSessionFile::declaration).collect(),
            now,
            now,
        )
        .map_err(Into::into)
    }

    pub(crate) fn materialize(
        &self,
        ownership: &mut MaterializationOwnershipRecord,
        profile: &NodeSessionMaterializationProfile,
        now: u64,
    ) -> Result<Vec<EnvMutation>, SessionEnvironmentMaterializeError> {
        ownership.validate()?;
        if ownership.state != MaterializationState::Preparing
            || ownership.root.parent() != Some(self.root.as_path())
            || ownership.root.file_name() != Some(OsStr::new(ownership.id.as_str()))
            || ownership.declared_paths
                != profile.0.files.iter().map(NodeSessionFile::declaration).collect::<Vec<_>>()
        {
            return Err(SessionEnvironmentMaterializeError::OwnershipMismatch);
        }
        let materialization_root = ownership.root.clone();
        if let Err(error) = secure_create_directory(&materialization_root) {
            mark_materialization_failure(ownership, now, !materialization_root.exists());
            return Err(error.into());
        }
        let marker = materialization_owner_marker(&ownership.id, &ownership.environment_profile);
        if let Err(error) = secure_create_file(&materialization_root.join(MATERIALIZATION_OWNER_MARKER), marker.as_bytes()) {
            let absence_proven = fs::remove_dir(&materialization_root).is_ok()
                && !materialization_root.exists();
            mark_materialization_failure(ownership, now, absence_proven);
            return Err(error.into());
        }
        let result = self.populate(&materialization_root, profile);
        if let Err(error) = result {
            let absence_proven = remove_owned_tree(&materialization_root, marker.as_bytes()).is_ok()
                && !materialization_root.exists();
            mark_materialization_failure(ownership, now, absence_proven);
            return Err(error);
        }
        ownership.mark_ready(now)?;
        result
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &self,
        id: MaterializationId,
        environment_profile: ResolvedEnvironmentProfileReceipt,
        owner: MaterializationOwner,
        managed_lease_id: Option<ManagedWorktreeLeaseId>,
        profile: &NodeSessionMaterializationProfile,
        now: u64,
    ) -> Result<PreparedSessionEnvironment, SessionEnvironmentMaterializeError> {
        let mut ownership = self.begin(
            id,
            environment_profile,
            owner,
            managed_lease_id,
            profile,
            now,
        )?;
        let environment = self.materialize(&mut ownership, profile, now)?;
        Ok(PreparedSessionEnvironment { environment, ownership })
    }

    fn populate(
        &self,
        materialization_root: &Path,
        profile: &NodeSessionMaterializationProfile,
    ) -> Result<Vec<EnvMutation>, SessionEnvironmentMaterializeError> {
        let mut class_roots = Vec::new();
        for class in [
            NodeSessionPathClass::ProviderHome,
            NodeSessionPathClass::Config,
            NodeSessionPathClass::Cache,
            NodeSessionPathClass::Data,
            NodeSessionPathClass::State,
            NodeSessionPathClass::Tmp,
        ] {
            let path = materialization_root.join(class.directory_name());
            secure_create_directory(&path)?;
            class_roots.push((class, path));
        }
        let path_for = |class| class_roots.iter().find(|(candidate, _)| *candidate == class).map(|(_, path)| path).expect("all classes exist");
        let environment = self.resolve_overlay(profile, &|class| path_for(class).clone())?;
        for file in &profile.0.files {
            let (class, relative_path, bytes) = match file {
                NodeSessionFile::Generated { class, relative_path, contents } => (*class, relative_path, contents.clone()),
                NodeSessionFile::Secret { class, relative_path, reference } => (*class, relative_path, self.resolver.resolve(reference)?.into_file_bytes()),
            };
            let destination = path_for(class).join(relative_path);
            secure_create_parent_directories(path_for(class), relative_path.parent())?;
            secure_create_file(&destination, &bytes)?;
        }
        Ok(environment)
    }

    pub(crate) fn resolve_environment(
        &self,
        ownership: &MaterializationOwnershipRecord,
        profile: &NodeSessionMaterializationProfile,
    ) -> Result<Vec<EnvMutation>, SessionEnvironmentMaterializeError> {
        if ownership.state != MaterializationState::Ready
            || ownership.declared_paths
                != profile.0.files.iter().map(NodeSessionFile::declaration).collect::<Vec<_>>()
        {
            return Err(SessionEnvironmentMaterializeError::OwnershipMismatch);
        }
        self.revalidate(ownership)?;
        let path_for = |class: NodeSessionPathClass| ownership.root.join(class.directory_name());
        for file in &profile.0.files {
            if let NodeSessionFile::Secret { class, relative_path, reference } = file {
                let bytes = self.resolver.resolve(reference)?.into_file_bytes();
                secure_replace_file(&path_for(*class).join(relative_path), &bytes)?;
            }
        }
        self.resolve_overlay(profile, &|class| path_for(class))
    }

    fn resolve_overlay<P>(
        &self,
        profile: &NodeSessionMaterializationProfile,
        path_for: &P,
    ) -> Result<Vec<EnvMutation>, SessionEnvironmentMaterializeError>
    where
        P: Fn(NodeSessionPathClass) -> PathBuf,
    {
        let mut environment = Vec::with_capacity(profile.0.environment.len() + profile.0.path_bindings.len());
        for mutation in &profile.0.environment {
            environment.push(match mutation {
                NodeSessionEnvironmentMutation::SetNonSecret { key, value } => EnvMutation { key: OsString::from(key), value: Some(OsString::from(value)) },
                NodeSessionEnvironmentMutation::SetSecret { key, reference } => EnvMutation {
                    key: OsString::from(key),
                    value: Some(self.resolver.resolve(reference)?.into_environment_value()?),
                },
                NodeSessionEnvironmentMutation::Remove { key } => EnvMutation { key: OsString::from(key), value: None },
            });
        }
        for binding in &profile.0.path_bindings {
            environment.push(EnvMutation { key: OsString::from(&binding.key), value: Some(path_for(binding.class).into_os_string()) });
        }
        Ok(environment)
    }

    pub(crate) fn revalidate(
        &self,
        ownership: &MaterializationOwnershipRecord,
    ) -> Result<(), SessionEnvironmentMaterializeError> {
        ownership.validate()?;
        if ownership.state != MaterializationState::Ready
            || ownership.root.parent() != Some(self.root.as_path())
            || ownership.root.file_name() != Some(OsStr::new(ownership.id.as_str()))
        {
            return Err(SessionEnvironmentMaterializeError::OwnershipMismatch);
        }
        validate_secure_directory(&ownership.root)?;
        let expected = materialization_owner_marker(&ownership.id, &ownership.environment_profile);
        let marker = ownership.root.join(MATERIALIZATION_OWNER_MARKER);
        validate_secure_file(&marker)?;
        let mut bytes = Vec::new();
        File::open(marker)?.take(4096).read_to_end(&mut bytes)?;
        if bytes != expected.as_bytes() {
            return Err(SessionEnvironmentMaterializeError::OwnershipMismatch);
        }
        for class in [
            NodeSessionPathClass::ProviderHome,
            NodeSessionPathClass::Config,
            NodeSessionPathClass::Cache,
            NodeSessionPathClass::Data,
            NodeSessionPathClass::State,
            NodeSessionPathClass::Tmp,
        ] {
            validate_secure_directory(&ownership.root.join(class.directory_name()))?;
        }
        for declaration in &ownership.declared_paths {
            validate_secure_file(
                &ownership.root
                    .join(declaration.class.directory_name())
                    .join(&declaration.relative_path),
            )?;
        }
        Ok(())
    }

    pub(crate) fn cleanup(&self, ownership: &MaterializationOwnershipRecord) -> Result<(), SessionEnvironmentMaterializeError> {
        ownership.validate()?;
        if ownership.root.parent() != Some(self.root.as_path())
            || ownership.root.file_name() != Some(OsStr::new(ownership.id.as_str()))
        {
            return Err(SessionEnvironmentMaterializeError::OwnershipMismatch);
        }
        if !ownership.root.exists() && ownership.state == MaterializationState::CleanupRequired {
            return Ok(());
        }
        let expected = materialization_owner_marker(&ownership.id, &ownership.environment_profile);
        remove_owned_tree(&ownership.root, expected.as_bytes())?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum SessionEnvironmentMaterializeError {
    #[error("materialization root is invalid")]
    InvalidRoot,
    #[error("materialization ownership does not match the configured root")]
    OwnershipMismatch,
    #[error("secret is unavailable")]
    SecretUnavailable,
    #[error("secret access is denied")]
    SecretDenied,
    #[error("secret value is invalid for its destination")]
    InvalidSecretValue,
    #[error("materialization filesystem operation failed")]
    Filesystem(#[source] io::Error),
    #[error("materialization ownership record is invalid")]
    Record(#[source] MaterializationRecordError),
}

impl From<NodeSecretResolveError> for SessionEnvironmentMaterializeError {
    fn from(value: NodeSecretResolveError) -> Self {
        match value {
            NodeSecretResolveError::Unavailable => Self::SecretUnavailable,
            NodeSecretResolveError::Denied => Self::SecretDenied,
        }
    }
}

impl From<io::Error> for SessionEnvironmentMaterializeError {
    fn from(value: io::Error) -> Self { Self::Filesystem(value) }
}

impl From<MaterializationRecordError> for SessionEnvironmentMaterializeError {
    fn from(value: MaterializationRecordError) -> Self { Self::Record(value) }
}

fn valid_opaque_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_materialization_id(value: &str) -> bool {
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

fn validate_environment_key(key: &str) -> Result<(), NodeSessionMaterializationProfileError> {
    if key.is_empty()
        || key.len() > 256
        || key.bytes().any(|byte| byte == 0 || byte == b'=' || !byte.is_ascii())
    {
        return Err(NodeSessionMaterializationProfileError::InvalidEnvironmentKey);
    }
    Ok(())
}

fn normalized_environment_key(key: &str) -> String { key.to_ascii_uppercase() }

fn validate_relative_path(path: &Path) -> Result<(), NodeSessionMaterializationProfileError> {
    let Some(encoded) = path.to_str() else {
        return Err(NodeSessionMaterializationProfileError::InvalidRelativePath);
    };
    if path.as_os_str().is_empty()
        || encoded.len() > MAX_SESSION_MATERIALIZATION_RELATIVE_PATH_BYTES
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(NodeSessionMaterializationProfileError::InvalidRelativePath);
    }
    Ok(())
}

fn checked_timestamp(created: u64, now: u64) -> Result<u64, MaterializationRecordError> {
    if now < created { Err(MaterializationRecordError::InvalidRecord) } else { Ok(now) }
}

fn mark_materialization_failure(
    ownership: &mut MaterializationOwnershipRecord,
    now: u64,
    absence_proven: bool,
) {
    let transition = if absence_proven {
        ownership.mark_cleanup_required(now)
    } else {
        ownership.mark_recovery_required(now)
    };
    if transition.is_err() {
        ownership.state = MaterializationState::RecoveryRequired;
        ownership.updated_at_unix_ms = ownership.created_at_unix_ms.max(now);
    }
}

fn materialization_owner_marker(id: &MaterializationId, profile: &ResolvedEnvironmentProfileReceipt) -> String {
    format!("{}\n{}\n{}\n", id.as_str(), profile.profile_id.as_str(), profile.profile_revision.as_str())
}

fn ensure_materialization_root(root: &Path) -> io::Result<()> {
    if root.exists() {
        validate_secure_directory(root)
    } else {
        secure_create_directory(root)
    }
}

fn secure_create_parent_directories(base: &Path, parent: Option<&Path>) -> io::Result<()> {
    let Some(parent) = parent else { return Ok(()); };
    let mut current = base.to_path_buf();
    for component in parent.components() {
        let Component::Normal(name) = component else { return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid materialization path")); };
        current.push(name);
        match secure_create_directory(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => validate_secure_directory(&current)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn verify_or_create_exact_file(path: &Path, expected: &[u8]) -> io::Result<()> {
    match secure_create_file(path, expected) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_secure_file(path)?;
            let mut bytes = Vec::new();
            File::open(path)?.take(4096).read_to_end(&mut bytes)?;
            if bytes == expected { Ok(()) } else { Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization marker mismatch")) }
        }
        Err(error) => Err(error),
    }
}

fn secure_replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    validate_secure_file(path)?;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new().write(true).truncate(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)?
    };
    #[cfg(windows)]
    let mut file = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new().write(true).truncate(true).custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT).open(path)?
    };
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_owned_tree(root: &Path, expected_marker: &[u8]) -> io::Result<()> {
    validate_secure_directory(root)?;
    let marker = root.join(MATERIALIZATION_OWNER_MARKER);
    validate_secure_file(&marker)?;
    let mut bytes = Vec::new();
    File::open(&marker)?.take(4096).read_to_end(&mut bytes)?;
    if bytes != expected_marker {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization owner marker mismatch"));
    }
    remove_tree_no_links(root)?;
    fs::remove_dir(root)
}

fn remove_tree_no_links(directory: &Path) -> io::Result<()> {
    validate_secure_directory(directory)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization contains link-like entry"));
        }
        if metadata.is_dir() {
            remove_tree_no_links(&path)?;
            fs::remove_dir(&path)?;
        } else if metadata.is_file() {
            validate_secure_file(&path)?;
            fs::remove_file(&path)?;
        } else {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization contains unsupported entry"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn secure_create_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    validate_secure_directory(path)
}

#[cfg(unix)]
fn validate_secure_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization directory is not owner-only"));
    }
    Ok(())
}

#[cfg(unix)]
fn secure_create_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization file is not owner-only"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_secure_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization file is not owner-only"));
    }
    Ok(())
}

#[cfg(unix)]
fn is_reparse_point(_: &fs::Metadata) -> bool { false }

#[cfg(windows)]
fn secure_create_directory(path: &Path) -> io::Result<()> { windows_secure::create_directory(path) }
#[cfg(windows)]
fn validate_secure_directory(path: &Path) -> io::Result<()> { windows_secure::validate_path(path, true) }
#[cfg(windows)]
fn secure_create_file(path: &Path, bytes: &[u8]) -> io::Result<()> { windows_secure::create_file(path, bytes) }
#[cfg(windows)]
fn validate_secure_file(path: &Path) -> io::Result<()> { windows_secure::validate_path(path, false) }
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

struct MaterializationRootLock { file: Option<File>, #[cfg(windows)] path: PathBuf }

impl MaterializationRootLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        verify_or_create_exact_file(path, b"")?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?
        };
        #[cfg(unix)]
        let file = {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err(io::Error::last_os_error()); }
            file
        };
        Ok(Self { file: Some(file), #[cfg(windows)] path: path.to_path_buf() })
    }
}

impl Drop for MaterializationRootLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = self.file.as_ref() {
            use std::os::fd::AsRawFd;
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN); }
        }
        drop(self.file.take());
        #[cfg(windows)]
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn path_to_opaque(path: &Path) -> io::Result<OpaqueHostPath> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        OpaqueHostPath::unix_bytes(path.as_os_str().as_bytes().to_vec()).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    #[cfg(windows)]
    {
        OpaqueHostPath::utf8(path.to_string_lossy().into_owned()).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

pub(crate) fn opaque_to_path(path: &OpaqueHostPath) -> io::Result<PathBuf> {
    if let Some(value) = path.as_utf8() { return Ok(PathBuf::from(value)); }
    #[cfg(unix)]
    if let Some(value) = path.as_unix_bytes() {
        use std::os::unix::ffi::OsStringExt;
        return Ok(PathBuf::from(OsString::from_vec(value.to_vec())));
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "materialization path encoding is unsupported"))
}

#[cfg(windows)]
mod windows_secure {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
        WinCreatorOwnerRightsSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileAttributesW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ALL_ACCESS,
    };

    const OWNER_ONLY_SDDL: &str = "D:P(A;;FA;;;OW)";

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl SecurityDescriptor {
        fn new() -> io::Result<Self> {
            let sddl = wide(OsStr::new(OWNER_ONLY_SDDL));
            let mut descriptor = std::ptr::null_mut();
            if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
        fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
        }
    }
    impl Drop for SecurityDescriptor { fn drop(&mut self) { unsafe { LocalFree(self.0); } } }

    pub(super) fn create_directory(path: &Path) -> io::Result<()> {
        let path = wide(path.as_os_str());
        let mut descriptor = SecurityDescriptor::new()?;
        let attributes = descriptor.attributes();
        if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) { return Err(io::Error::new(io::ErrorKind::AlreadyExists, error)); }
            return Err(error);
        }
        validate_wide_path(&path, true)
    }

    pub(super) fn create_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = wide(path.as_os_str());
        let mut descriptor = SecurityDescriptor::new()?;
        let attributes = descriptor.attributes();
        let handle = unsafe { CreateFileW(path.as_ptr(), FILE_GENERIC_WRITE, 0, &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        let mut file = unsafe { File::from_raw_handle(handle as _) };
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    pub(super) fn validate_path(path: &Path, directory: bool) -> io::Result<()> {
        validate_wide_path(&wide(path.as_os_str()), directory)
    }

    fn validate_wide_path(path: &[u16], directory: bool) -> io::Result<()> {
        let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attributes == u32::MAX || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization path is reparse or unavailable"));
        }
        let flags = FILE_FLAG_OPEN_REPARSE_POINT | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 };
        let handle: HANDLE = unsafe { CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        unsafe { CloseHandle(handle); }
        validate_owner_only_dacl(path)
    }

    fn validate_owner_only_dacl(path: &[u16]) -> io::Result<()> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 { return Err(io::Error::from_raw_os_error(status as i32)); }
        let result = (|| {
            if owner.is_null() || dacl.is_null() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization DACL is missing"));
            }
            let mut control = 0u16;
            let mut revision = 0u32;
            if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
                || control & SE_DACL_PROTECTED == 0
            {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization DACL is not protected"));
            }
            let mut information = ACL_SIZE_INFORMATION::default();
            if unsafe {
                GetAclInformation(
                    dacl,
                    &mut information as *mut _ as *mut _,
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            } == 0 || information.AceCount != 1 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization DACL is not owner-only"));
            }
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
                return Err(io::Error::last_os_error());
            }
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let sid = &allowed.SidStart as *const u32 as *mut _;
            let mut owner_rights = [0u8; SECURITY_MAX_SID_SIZE as usize];
            let mut owner_rights_len = owner_rights.len() as u32;
            if unsafe {
                CreateWellKnownSid(
                    WinCreatorOwnerRightsSid,
                    std::ptr::null_mut(),
                    owner_rights.as_mut_ptr() as *mut _,
                    &mut owner_rights_len,
                )
            } == 0 {
                return Err(io::Error::last_os_error());
            }
            if allowed.Header.AceType != 0
                || allowed.Header.AceFlags != 0
                || allowed.Mask != FILE_ALL_ACCESS
                || (unsafe { EqualSid(owner, sid) } == 0
                    && unsafe { EqualSid(owner_rights.as_mut_ptr() as *mut _, sid) } == 0)
            {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "materialization DACL is not owner-only"));
            }
            Ok(())
        })();
        unsafe { LocalFree(descriptor); }
        result
    }

    fn wide(value: &OsStr) -> Vec<u16> { value.encode_wide().chain(std::iter::once(0)).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    struct FakeResolver;
    impl NodeSecretResolver for FakeResolver {
        fn resolve(&self, reference: &NodeSecretReference) -> Result<NodeSecretValue, NodeSecretResolveError> {
            if reference.as_str() == "fixture-token" { NodeSecretValue::text("fixture-secret").map_err(|_| NodeSecretResolveError::Unavailable) } else { Err(NodeSecretResolveError::Unavailable) }
        }
    }

    struct UnavailableResolver;
    impl NodeSecretResolver for UnavailableResolver {
        fn resolve(&self, _: &NodeSecretReference) -> Result<NodeSecretValue, NodeSecretResolveError> {
            Err(NodeSecretResolveError::Unavailable)
        }
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("gate4agent-materializer-{}-{}", std::process::id(), NEXT_TEST.fetch_add(1, Ordering::Relaxed)))
    }

    fn receipt() -> ResolvedEnvironmentProfileReceipt {
        ResolvedEnvironmentProfileReceipt {
            profile_id: crate::protocol::SpawnEnvironmentProfileId::new("fixture").unwrap(),
            profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new("r1").unwrap(),
        }
    }

    fn owner() -> MaterializationOwner {
        MaterializationOwner::Session {
            incarnation_id: NodeIncarnationId::from_bytes([7; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            instance_id: AgentInstanceId(4),
            generation: SessionGeneration(2),
        }
    }

    #[test]
    fn materialization_profile_rejects_traversal_prefix_collisions_and_normalized_env_duplicates() {
        assert_eq!(NodeSessionFile::generated(NodeSessionPathClass::Config, "../escape", vec![]).err(), Some(NodeSessionMaterializationProfileError::InvalidRelativePath));
        let duplicate = NodeSessionMaterializationProfile::new(
            vec![NodeSessionEnvironmentMutation::Remove { key: "Path".to_owned() }],
            vec![NodeSessionPathBinding::new("PATH", NodeSessionPathClass::ProviderHome).unwrap()],
            vec![],
        );
        assert_eq!(duplicate.err(), Some(NodeSessionMaterializationProfileError::DuplicateEnvironmentKey));
        let collision = NodeSessionMaterializationProfile::new(
            vec![], vec![], vec![
                NodeSessionFile::generated(NodeSessionPathClass::Config, "a", vec![]).unwrap(),
                NodeSessionFile::generated(NodeSessionPathClass::Config, "a/b", vec![]).unwrap(),
            ],
        );
        assert_eq!(collision.err(), Some(NodeSessionMaterializationProfileError::ConflictingFilePath));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let non_utf8 = PathBuf::from(OsString::from_vec(vec![b'a', 0xff]));
            assert_eq!(
                NodeSessionFile::generated(NodeSessionPathClass::Config, non_utf8, vec![]).err(),
                Some(NodeSessionMaterializationProfileError::InvalidRelativePath),
            );
        }
    }

    #[test]
    fn materialization_id_rejects_noncanonical_case() {
        assert_eq!(
            MaterializationId::new("Mat-a").err(),
            Some(MaterializationRecordError::InvalidId),
        );
        assert_eq!(
            MaterializationId::new("mat-a").unwrap().as_str(),
            "mat-a",
        );
    }

    #[test]
    fn secret_reference_and_content_containers_have_no_debug_surface() {
        fn assert_no_debug<T>() {
            let name = std::any::type_name::<T>();
            assert!(!name.is_empty());
        }
        assert_no_debug::<NodeSecretReference>();
        assert_no_debug::<NodeSecretValue>();
        assert_no_debug::<NodeSessionEnvironmentMutation>();
        assert_no_debug::<NodeSessionFile>();
        assert_no_debug::<NodeSessionMaterializationProfile>();
    }

    #[test]
    fn owner_only_materialization_resolves_and_cleans_without_persisting_secret_content() {
        let root = temp_root();
        let materializer = SessionEnvironmentMaterializer::new(root.clone(), Arc::new(FakeResolver)).unwrap();
        let profile = NodeSessionMaterializationProfile::new(
            vec![NodeSessionEnvironmentMutation::SetSecret { key: "FIXTURE_TOKEN".to_owned(), reference: NodeSecretReference::new("fixture-token").unwrap() }],
            vec![NodeSessionPathBinding::new("FIXTURE_HOME", NodeSessionPathClass::ProviderHome).unwrap()],
            vec![NodeSessionFile::secret(NodeSessionPathClass::Config, "auth/token", NodeSecretReference::new("fixture-token").unwrap()).unwrap()],
        ).unwrap();
        let prepared = materializer.prepare(MaterializationId::new("fixture-1").unwrap(), receipt(), owner(), None, &profile, 10).unwrap();
        assert_eq!(prepared.environment().len(), 2);
        let (_, ownership) = prepared.into_parts();
        assert!(ownership.root().join("config/auth/token").is_file());
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(ownership.root()).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(fs::metadata(ownership.root().join("config/auth/token")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        materializer.cleanup(&ownership).unwrap();
        assert!(!ownership.root().exists());
        drop(materializer);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_materialization_retains_cleanup_state_until_absence_is_reconciled() {
        let root = temp_root();
        let materializer = SessionEnvironmentMaterializer::new(root.clone(), Arc::new(UnavailableResolver)).unwrap();
        let profile = NodeSessionMaterializationProfile::new(
            vec![NodeSessionEnvironmentMutation::SetSecret {
                key: "FIXTURE_TOKEN".to_owned(),
                reference: NodeSecretReference::new("opaque-reference-never-in-errors").unwrap(),
            }],
            vec![],
            vec![],
        ).unwrap();
        let mut ownership = materializer.begin(
            MaterializationId::new("fixture-failure").unwrap(),
            receipt(),
            owner(),
            None,
            &profile,
            20,
        ).unwrap();
        let error = materializer.materialize(&mut ownership, &profile, 21).unwrap_err();
        assert_eq!(ownership.state(), MaterializationState::CleanupRequired);
        assert!(!ownership.root().exists());
        assert!(!error.to_string().contains("opaque-reference-never-in-errors"));
        materializer.cleanup(&ownership).unwrap();
        drop(materializer);
        let _ = fs::remove_dir_all(root);
    }
}
