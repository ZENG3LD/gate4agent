use crate::protocol::{
    AgentId, ManagedSessionRecord, ManagedSessionState, NodeId, OpaqueHostPath,
    ResolvedEnvironmentProfileReceipt, SessionAddress, SessionMode, SessionRecordId, WorkspaceId,
    MAX_NODE_TEXT_BYTES, MAX_SESSION_DISPLAY_NAME_BYTES, MAX_WORKSPACE_ROOT_BYTES,
    NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2, NODE_STATE_SCHEMA_V3, NODE_STATE_SCHEMA_V4,
    NODE_STATE_SCHEMA_V5,
    NODE_STATE_SCHEMA_V6,
};
use crate::worktree_service::ManagedWorktreeLeaseRecord;
use crate::session_environment::{
    opaque_to_path, path_to_opaque, MaterializationId, MaterializationOwner,
    MaterializationOwnershipRecord, MaterializationState, MaterializedPathDeclaration,
    MAX_SESSION_MATERIALIZATIONS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PERSISTED_WORKSPACES: usize = 256;
const MAX_PERSISTED_MANAGED_WORKTREE_BRANCH_BYTES: usize = 512;
pub(crate) const MAX_MANAGED_SESSION_RECORDS: usize = 4_096;
const BACKUP_ROTATION_WARNING: &str = "durable-state-backup-rotation-failed";
const CORRUPT_PRIMARY_PRESERVED_WARNING: &str = "durable-state-corrupt-primary-preserved";
pub(crate) const DURABLE_STATE_COMMIT_FAILED: &str = "durable-state-commit-failed";
pub(crate) const DURABLE_STATE_RECOVERY_WARNING: &str = "durable-state-recovery-required";
pub(crate) const DURABLE_STATE_WORKSPACE_WARNING: &str =
    "durable-state-workspace-unavailable";
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(1);

pub(crate) type ProviderSessionSemanticKey = (String, u8, String);

pub(crate) fn provider_session_semantic_key(
    provider: &AgentId,
    identity: &gate4agent_types::ProviderSessionIdentity,
) -> ProviderSessionSemanticKey {
    let key = match identity.key {
        gate4agent_types::ProviderSessionKey::SessionId => 0,
        gate4agent_types::ProviderSessionKey::ConversationId => 1,
    };
    (provider.as_str().to_owned(), key, identity.id.clone())
}

pub(crate) fn same_provider_session(
    left_provider: &AgentId,
    left: &gate4agent_types::ProviderSessionIdentity,
    right_provider: &AgentId,
    right: &gate4agent_types::ProviderSessionIdentity,
) -> bool {
    left_provider == right_provider && left.key == right.key && left.id == right.id
}

pub(crate) fn sanitized_persistence_summary(message: &str) -> String {
    match message {
        BACKUP_ROTATION_WARNING
        | CORRUPT_PRIMARY_PRESERVED_WARNING
        | DURABLE_STATE_COMMIT_FAILED
        | DURABLE_STATE_RECOVERY_WARNING
        | DURABLE_STATE_WORKSPACE_WARNING => message.to_owned(),
        _ => DURABLE_STATE_COMMIT_FAILED.to_owned(),
    }
}

pub(crate) struct StatePathLock {
    file: Option<File>,
    #[cfg(windows)]
    path: PathBuf,
}

impl StatePathLock {
    pub(crate) fn acquire(path: Option<&Path>) -> io::Result<Option<Self>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "durable state path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        secure_state_directory(parent)?;
        let lock_path = sibling_path(path, "lock");
        #[cfg(windows)]
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lock_path)
            .map_err(|error| {
                if error.raw_os_error() == Some(32) {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "durable state path is already owned by another node process",
                    )
                } else {
                    error
                }
            })?;
        #[cfg(unix)]
        let file = acquire_unix_state_lock(&lock_path)?;
        Ok(Some(Self {
            file: Some(file),
            #[cfg(windows)]
            path: lock_path,
        }))
    }
}

impl Drop for StatePathLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = self.file.as_ref() {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        drop(self.file.take());
        #[cfg(windows)]
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
use std::os::fd::AsRawFd;

#[cfg(unix)]
fn secure_state_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "durable state directory is not owned by the current user",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "durable state directory must have mode 0700",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_unix_state_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "durable state lock is not an owner-controlled regular file",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "durable state path is already owned by another node process",
            ));
        }
        return Err(error);
    }
    Ok(file)
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct LoadedNodeState {
    pub workspaces: BTreeMap<WorkspaceId, String>,
    pub records: Vec<ManagedSessionRecord>,
    pub managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
    pub managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
    pub materializations: Vec<MaterializationOwnershipRecord>,
    pub warning: Option<String>,
}

impl LoadedNodeState {
    fn empty() -> Self {
        Self {
            workspaces: BTreeMap::new(),
            records: Vec::new(),
            managed_worktrees: Vec::new(),
            managed_worktree_tombstones: Vec::new(),
            materializations: Vec::new(),
            warning: None,
        }
    }
}

impl fmt::Debug for LoadedNodeState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedNodeState")
            .field("workspace_count", &self.workspaces.len())
            .field("record_count", &self.records.len())
            .field("managed_worktree_count", &self.managed_worktrees.len())
            .field("managed_worktree_tombstone_count", &self.managed_worktree_tombstones.len())
            .field("materialization_count", &self.materializations.len())
            .field("warning", &self.warning)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV1 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV1>,
    session_records: Vec<PersistedManagedSessionRecordV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV2 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV2>,
    session_records: Vec<PersistedManagedSessionRecordV2>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV3 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV2>,
    session_records: Vec<PersistedManagedSessionRecordV3>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV4 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV2>,
    session_records: Vec<PersistedManagedSessionRecordV3>,
    managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
    managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV5 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV2>,
    session_records: Vec<PersistedManagedSessionRecordV5>,
    managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
    managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeStateV6 {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspaceV2>,
    session_records: Vec<PersistedManagedSessionRecordV5>,
    managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
    managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
    materializations: Vec<PersistedMaterializationRecordV6>,
}

struct DecodedNodeState {
    node_id: NodeId,
    workspaces: Vec<(WorkspaceId, String)>,
    session_records: Vec<ManagedSessionRecord>,
    managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
    managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
    materializations: Vec<MaterializationOwnershipRecord>,
}

#[derive(Deserialize)]
struct PersistedNodeStateHeader {
    version: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateLoadRefusal {
    UnsupportedSchema,
    PathSemanticsUnsupported,
    NodeIdentityMismatch,
}

#[derive(Debug)]
enum StateLoadRefusalError {
    UnsupportedSchema(u16),
    PathSemanticsUnsupported,
    NodeIdentityMismatch,
}

impl StateLoadRefusalError {
    fn refusal(&self) -> StateLoadRefusal {
        match self {
            Self::UnsupportedSchema(_) => StateLoadRefusal::UnsupportedSchema,
            Self::PathSemanticsUnsupported => StateLoadRefusal::PathSemanticsUnsupported,
            Self::NodeIdentityMismatch => StateLoadRefusal::NodeIdentityMismatch,
        }
    }
}

impl fmt::Display for StateLoadRefusalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported durable state version {version}")
            }
            Self::PathSemanticsUnsupported => {
                formatter.write_str("durable state path semantics are unsupported on this host")
            }
            Self::NodeIdentityMismatch => {
                formatter.write_str("durable state belongs to a different node")
            }
        }
    }
}

impl StdError for StateLoadRefusalError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedWorkspaceV1 {
    workspace_id: WorkspaceId,
    canonical_root: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedWorkspaceV2 {
    workspace_id: WorkspaceId,
    canonical_root: OpaqueHostPath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedManagedSessionRecordV1 {
    record_id: SessionRecordId,
    display_name: String,
    provider: LegacyAgentProvider,
    mode: SessionMode,
    state: ManagedSessionState,
    workspace_id: WorkspaceId,
    canonical_root: String,
    provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    active_session: Option<SessionAddress>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedManagedSessionRecordV2 {
    record_id: SessionRecordId,
    display_name: String,
    provider: LegacyAgentProvider,
    mode: SessionMode,
    state: ManagedSessionState,
    workspace_id: WorkspaceId,
    canonical_root: OpaqueHostPath,
    provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    active_session: Option<SessionAddress>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum LegacyAgentProvider {
    Claude,
    Codex,
    Kimi,
}

impl LegacyAgentProvider {
    fn into_agent_id(self) -> AgentId {
        let value = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        };
        AgentId::new(value).expect("legacy provider IDs remain valid")
    }

    #[cfg(test)]
    fn from_agent_id(provider: &AgentId) -> Self {
        match provider.as_str() {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "kimi" => Self::Kimi,
            value => panic!("{value} is not a legacy provider ID"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedManagedSessionRecordV3 {
    record_id: SessionRecordId,
    display_name: String,
    provider: AgentId,
    mode: SessionMode,
    state: ManagedSessionState,
    workspace_id: WorkspaceId,
    canonical_root: OpaqueHostPath,
    provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    active_session: Option<SessionAddress>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedManagedSessionRecordV5 {
    record_id: SessionRecordId,
    display_name: String,
    provider: AgentId,
    mode: SessionMode,
    state: ManagedSessionState,
    workspace_id: WorkspaceId,
    canonical_root: OpaqueHostPath,
    provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    active_session: Option<SessionAddress>,
    environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_error: Option<String>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedMaterializationRecordV6 {
    materialization_id: MaterializationId,
    environment_profile: ResolvedEnvironmentProfileReceipt,
    owner: MaterializationOwner,
    managed_lease_id: Option<crate::protocol::ManagedWorktreeLeaseId>,
    state: MaterializationState,
    root: OpaqueHostPath,
    provider_home: OpaqueHostPath,
    declared_paths: Vec<MaterializedPathDeclaration>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
}

pub(crate) fn load(path: Option<&Path>, expected_node_id: &NodeId) -> io::Result<LoadedNodeState> {
    let Some(path) = path else {
        return Ok(LoadedNodeState::empty());
    };
    let backup = sibling_path(path, "bak");
    let pending_backup = sibling_path(path, "pending-backup");
    if !path.exists() {
        if !pending_backup.exists() && !backup.exists() {
            return Ok(LoadedNodeState::empty());
        }
        return recover_from_copies(
            path,
            &pending_backup,
            &backup,
            expected_node_id,
            "is missing".to_owned(),
        );
    }
    match load_one(path, expected_node_id) {
        Ok(loaded) => Ok(loaded),
        Err(primary_error) if is_authoritative_state_error(&primary_error) => {
            Err(primary_error)
        }
        Err(primary_error) => recover_from_copies(
            path,
            &pending_backup,
            &backup,
            expected_node_id,
            format!("is invalid: {primary_error}"),
        ),
    }
}

fn recover_from_copies(
    primary: &Path,
    pending_backup: &Path,
    backup: &Path,
    expected_node_id: &NodeId,
    _primary_problem: String,
) -> io::Result<LoadedNodeState> {
    for candidate in [pending_backup, backup] {
        if !candidate.exists() {
            continue;
        }
        match load_one(candidate, expected_node_id) {
            Ok(mut loaded) => {
                loaded.warning = Some(DURABLE_STATE_RECOVERY_WARNING.to_owned());
                return Ok(loaded);
            }
            Err(error) if is_authoritative_state_error(&error) => {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    let _ = primary;
    Err(invalid_data("durable-state-recovery-failed"))
}

fn load_one(path: &Path, expected_node_id: &NodeId) -> io::Result<LoadedNodeState> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_STATE_BYTES {
        return Err(invalid_data("durable state exceeds the bounded file size"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("durable state exceeds the bounded file size"));
    }
    let state = decode_persisted_state(&bytes)?;
    if &state.node_id != expected_node_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            StateLoadRefusalError::NodeIdentityMismatch,
        ));
    }
    if state.workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("durable state contains too many workspaces"));
    }
    if state.session_records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("durable state contains too many session records"));
    }

    let mut workspaces = BTreeMap::new();
    let mut roots = BTreeSet::new();
    for (workspace_id, canonical_root) in state.workspaces {
        validate_root(&canonical_root)?;
        if workspaces
            .insert(workspace_id.clone(), canonical_root.clone())
            .is_some()
        {
            return Err(invalid_data("durable state contains duplicate workspace IDs"));
        }
        if !roots.insert(crate::platform::root_identity(&canonical_root)) {
            return Err(invalid_data("durable state contains duplicate workspace roots"));
        }
    }

    let mut record_ids = BTreeSet::new();
    let mut provider_sessions = BTreeSet::new();
    let mut records = Vec::with_capacity(state.session_records.len());
    for mut record in state.session_records {
        if let Some(identity) = &mut record.provider_session {
            identity.transcript_path = None;
        }
        validate_record(&record)?;
        record.last_error = record
            .last_error
            .as_deref()
            .map(sanitized_record_error_summary);
        if !record_ids.insert(record.record_id.clone()) {
            return Err(invalid_data("durable state contains duplicate session record IDs"));
        }
        if let Some(identity) = &mut record.provider_session {
            let key = provider_session_semantic_key(&record.provider, identity);
            if !provider_sessions.insert(key) {
                return Err(invalid_data("durable state contains duplicate provider sessions"));
            }
        }
        record.active_session = None;
        record.state = if record.provider_session.is_some() {
            ManagedSessionState::Dormant
        } else {
            ManagedSessionState::Unavailable
        };
        records.push(record);
    }
    Ok(LoadedNodeState {
        workspaces,
        records,
        managed_worktrees: state.managed_worktrees,
        managed_worktree_tombstones: state.managed_worktree_tombstones,
        materializations: state.materializations,
        warning: None,
    })
}

fn decode_persisted_state(bytes: &[u8]) -> io::Result<DecodedNodeState> {
    let header: PersistedNodeStateHeader = serde_json::from_slice(bytes)
        .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
    match header.version {
        NODE_STATE_SCHEMA_V1 => {
            let state: PersistedNodeStateV1 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v1_state(state)
        }
        NODE_STATE_SCHEMA_V2 => {
            let state: PersistedNodeStateV2 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v2_state(state)
        }
        NODE_STATE_SCHEMA_V3 => {
            let state: PersistedNodeStateV3 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v3_state(state)
        }
        NODE_STATE_SCHEMA_V4 => {
            let state: PersistedNodeStateV4 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v4_state(state)
        }
        NODE_STATE_SCHEMA_V5 => {
            let state: PersistedNodeStateV5 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v5_state(state)
        }
        NODE_STATE_SCHEMA_V6 => {
            let state: PersistedNodeStateV6 = serde_json::from_slice(bytes)
                .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
            decode_v6_state(state)
        }
        version => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            StateLoadRefusalError::UnsupportedSchema(version),
        )),
    }
}

fn decode_v1_state(state: PersistedNodeStateV1) -> io::Result<DecodedNodeState> {
    Ok(DecodedNodeState {
        node_id: state.node_id,
        workspaces: state
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.workspace_id, workspace.canonical_root))
            .collect(),
        session_records: state
            .session_records
            .into_iter()
            .map(|record| {
                Ok(ManagedSessionRecord {
                    record_id: record.record_id,
                    display_name: record.display_name,
                    provider: record.provider.into_agent_id(),
                    mode: record.mode,
                    state: record.state,
                    workspace_id: record.workspace_id,
                    canonical_root: OpaqueHostPath::utf8(record.canonical_root)
                        .map_err(|error| invalid_data(format!(
                            "durable state contains an invalid workspace path: {error}"
                        )))?,
                    provider_session: record.provider_session,
                    active_session: record.active_session,
                    environment_profile: None,
                    created_at_unix_ms: record.created_at_unix_ms,
                    updated_at_unix_ms: record.updated_at_unix_ms,
                    last_error: record.last_error,
                })
            })
            .collect::<io::Result<_>>()?,
        managed_worktrees: Vec::new(),
        managed_worktree_tombstones: Vec::new(),
        materializations: Vec::new(),
    })
}

fn decode_v2_state(state: PersistedNodeStateV2) -> io::Result<DecodedNodeState> {
    Ok(DecodedNodeState {
        node_id: state.node_id,
        workspaces: state
            .workspaces
            .into_iter()
            .map(|workspace| {
                Ok((
                    workspace.workspace_id,
                    require_utf8_state_path(&workspace.canonical_root)?.to_owned(),
                ))
            })
            .collect::<io::Result<_>>()?,
        session_records: state
            .session_records
            .into_iter()
            .map(|record| {
                require_utf8_state_path(&record.canonical_root)?;
                Ok(ManagedSessionRecord {
                    record_id: record.record_id,
                    display_name: record.display_name,
                    provider: record.provider.into_agent_id(),
                    mode: record.mode,
                    state: record.state,
                    workspace_id: record.workspace_id,
                    canonical_root: record.canonical_root,
                    provider_session: record.provider_session,
                    active_session: record.active_session,
                    environment_profile: None,
                    created_at_unix_ms: record.created_at_unix_ms,
                    updated_at_unix_ms: record.updated_at_unix_ms,
                    last_error: record.last_error,
                })
            })
            .collect::<io::Result<_>>()?,
        managed_worktrees: Vec::new(),
        managed_worktree_tombstones: Vec::new(),
        materializations: Vec::new(),
    })
}

fn decode_v3_state(state: PersistedNodeStateV3) -> io::Result<DecodedNodeState> {
    Ok(DecodedNodeState {
        node_id: state.node_id,
        workspaces: state
            .workspaces
            .into_iter()
            .map(|workspace| {
                Ok((
                    workspace.workspace_id,
                    require_utf8_state_path(&workspace.canonical_root)?.to_owned(),
                ))
            })
            .collect::<io::Result<_>>()?,
        session_records: state
            .session_records
            .into_iter()
            .map(|record| {
                require_utf8_state_path(&record.canonical_root)?;
                Ok(ManagedSessionRecord {
                    record_id: record.record_id,
                    display_name: record.display_name,
                    provider: record.provider,
                    mode: record.mode,
                    state: record.state,
                    workspace_id: record.workspace_id,
                    canonical_root: record.canonical_root,
                    provider_session: record.provider_session,
                    active_session: record.active_session,
                    environment_profile: None,
                    created_at_unix_ms: record.created_at_unix_ms,
                    updated_at_unix_ms: record.updated_at_unix_ms,
                    last_error: record.last_error,
                })
            })
            .collect::<io::Result<_>>()?,
        managed_worktrees: Vec::new(),
        managed_worktree_tombstones: Vec::new(),
        materializations: Vec::new(),
    })
}

fn decode_v4_state(state: PersistedNodeStateV4) -> io::Result<DecodedNodeState> {
    let legacy = decode_v3_state(PersistedNodeStateV3 {
        version: NODE_STATE_SCHEMA_V3,
        node_id: state.node_id,
        workspaces: state.workspaces,
        session_records: state.session_records,
    })?;
    validate_managed_worktree_records(&state.managed_worktrees, false)?;
    validate_managed_worktree_records(&state.managed_worktree_tombstones, true)?;
    validate_managed_worktree_identity_sets(
        &state.managed_worktrees,
        &state.managed_worktree_tombstones,
    )?;
    Ok(DecodedNodeState {
        node_id: legacy.node_id,
        workspaces: legacy.workspaces,
        session_records: legacy.session_records,
        managed_worktrees: state.managed_worktrees,
        managed_worktree_tombstones: state.managed_worktree_tombstones,
        materializations: Vec::new(),
    })
}

fn decode_v5_state(state: PersistedNodeStateV5) -> io::Result<DecodedNodeState> {
    let mut environment_profiles = Vec::with_capacity(state.session_records.len());
    let legacy_records = state
        .session_records
        .into_iter()
        .map(|record| {
            environment_profiles.push(record.environment_profile);
            PersistedManagedSessionRecordV3 {
                record_id: record.record_id,
                display_name: record.display_name,
                provider: record.provider,
                mode: record.mode,
                state: record.state,
                workspace_id: record.workspace_id,
                canonical_root: record.canonical_root,
                provider_session: record.provider_session,
                active_session: record.active_session,
                created_at_unix_ms: record.created_at_unix_ms,
                updated_at_unix_ms: record.updated_at_unix_ms,
                last_error: record.last_error,
            }
        })
        .collect();
    let mut decoded = decode_v4_state(PersistedNodeStateV4 {
        version: NODE_STATE_SCHEMA_V4,
        node_id: state.node_id,
        workspaces: state.workspaces,
        session_records: legacy_records,
        managed_worktrees: state.managed_worktrees,
        managed_worktree_tombstones: state.managed_worktree_tombstones,
    })?;
    for (record, environment_profile) in decoded
        .session_records
        .iter_mut()
        .zip(environment_profiles)
    {
        record.environment_profile = environment_profile;
    }
    Ok(decoded)
}

fn decode_v6_state(state: PersistedNodeStateV6) -> io::Result<DecodedNodeState> {
    let mut decoded = decode_v5_state(PersistedNodeStateV5 {
        version: NODE_STATE_SCHEMA_V5,
        node_id: state.node_id,
        workspaces: state.workspaces,
        session_records: state.session_records,
        managed_worktrees: state.managed_worktrees,
        managed_worktree_tombstones: state.managed_worktree_tombstones,
    })?;
    let materializations = state.materializations.into_iter().map(|record| {
        MaterializationOwnershipRecord::from_persisted(
            record.materialization_id,
            record.environment_profile,
            record.owner,
            record.managed_lease_id,
            record.state,
            opaque_to_path(&record.root)?,
            opaque_to_path(&record.provider_home)?,
            record.declared_paths,
            record.created_at_unix_ms,
            record.updated_at_unix_ms,
        ).map_err(|error| invalid_data(format!("durable state materialization record is invalid: {error}")))
    }).collect::<io::Result<Vec<_>>>()?;
    validate_materialization_records(
        &materializations,
        &decoded.session_records,
        &decoded.managed_worktrees,
        &decoded.managed_worktree_tombstones,
    )?;
    decoded.materializations = materializations;
    Ok(decoded)
}

fn validate_materialization_records(
    records: &[MaterializationOwnershipRecord],
    session_records: &[ManagedSessionRecord],
    managed_worktrees: &[ManagedWorktreeLeaseRecord],
    managed_worktree_tombstones: &[ManagedWorktreeLeaseRecord],
) -> io::Result<()> {
    if records.len() > MAX_SESSION_MATERIALIZATIONS {
        return Err(invalid_data("durable state contains too many materializations"));
    }
    let known_records = session_records
        .iter()
        .map(|record| (&record.record_id, &record.workspace_id))
        .collect::<BTreeMap<_, _>>();
    let active_leases = managed_worktrees
        .iter()
        .map(|lease| (&lease.lease_id, lease))
        .collect::<BTreeMap<_, _>>();
    let tombstone_ids = managed_worktree_tombstones
        .iter()
        .map(|lease| &lease.lease_id)
        .collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for record in records {
        record.validate().map_err(|error| invalid_data(format!("durable state materialization record is invalid: {error}")))?;
        let (owner_key, record_workspace) = match record.owner() {
            MaterializationOwner::Session { incarnation_id, instance_id, generation } => {
                (
                    format!("session:{incarnation_id}:{}:{}", instance_id.0, generation.0),
                    None,
                )
            }
            MaterializationOwner::Record { record_id } => {
                let workspace_id = known_records.get(record_id).ok_or_else(|| {
                    invalid_data(
                        "durable state materialization references an unknown session record",
                    )
                })?;
                (format!("record:{}", record_id.as_str()), Some(*workspace_id))
            }
        };
        if let Some(lease_id) = record.managed_lease_id() {
            if tombstone_ids.contains(lease_id) {
                return Err(invalid_data(
                    "durable state materialization references a removed managed worktree lease",
                ));
            }
            let lease = active_leases.get(lease_id).ok_or_else(|| {
                invalid_data(
                    "durable state materialization references an unknown managed worktree lease",
                )
            })?;
            if let Some(record_workspace) = record_workspace {
                if record_workspace != &lease.workspace_id {
                    return Err(invalid_data(
                        "durable state materialization record and managed worktree lease disagree on workspace",
                    ));
                }
            }
        }
        if !ids.insert(record.id().clone())
            || !roots.insert(record.root().to_path_buf())
            || !owners.insert(owner_key)
        {
            return Err(invalid_data("durable state contains duplicate materialization ownership"));
        }
    }
    Ok(())
}

fn validate_managed_worktree_identity_sets(
    managed_worktrees: &[ManagedWorktreeLeaseRecord],
    managed_worktree_tombstones: &[ManagedWorktreeLeaseRecord],
) -> io::Result<()> {
    let active_ids = managed_worktrees
        .iter()
        .map(|lease| &lease.lease_id)
        .collect::<BTreeSet<_>>();
    if managed_worktree_tombstones
        .iter()
        .any(|lease| active_ids.contains(&lease.lease_id))
    {
        return Err(invalid_data(
            "durable state contains duplicate managed worktree leases",
        ));
    }
    Ok(())
}

fn validate_managed_worktree_records(
    records: &[ManagedWorktreeLeaseRecord],
    tombstones: bool,
) -> io::Result<()> {
    if records.len() > crate::protocol::MAX_MANAGED_WORKTREE_LEASES {
        return Err(invalid_data("durable state contains too many managed worktree leases"));
    }
    let mut lease_ids = BTreeSet::new();
    let mut workspace_ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut branches = BTreeSet::new();
    for lease in records {
        validate_root(&lease.target_root)?;
        let mut session_holders = BTreeSet::new();
        let mut record_holders = BTreeSet::new();
        if lease.branch.is_empty()
            || lease.branch.len() > MAX_PERSISTED_MANAGED_WORKTREE_BRANCH_BYTES
            || !valid_git_object_id(&lease.base_commit)
            || lease.expected_head.as_ref().is_some_and(|head| !valid_git_object_id(head))
            || lease.source_workspace_id == lease.workspace_id
            || lease.branch.chars().any(char::is_control)
            || lease.base_commit.chars().any(char::is_control)
            || lease.created_at_unix_ms > lease.updated_at_unix_ms
            || tombstones != (lease.state == crate::protocol::ManagedWorktreeLeaseState::Removed)
            || !lease_ids.insert(lease.lease_id.clone())
            || !workspace_ids.insert(lease.workspace_id.clone())
            || !roots.insert(crate::platform::root_identity(&lease.target_root))
            || !branches.insert(lease.branch.clone())
            || lease.session_holders.len() > gate4agent_types::CONTROL_SESSIONS_MAX
            || lease.record_holders.len() > MAX_MANAGED_SESSION_RECORDS
            || lease.session_holders.iter().any(|holder| !session_holders.insert((
                holder.incarnation_id,
                holder.instance_id,
                holder.generation,
            )))
            || lease.record_holders.iter().any(|holder| !record_holders.insert(holder.clone()))
            || (lease.state == crate::protocol::ManagedWorktreeLeaseState::Removed
                && (!lease.session_holders.is_empty() || !lease.record_holders.is_empty()))
            || (matches!(
                lease.state,
                crate::protocol::ManagedWorktreeLeaseState::Allocating
                    | crate::protocol::ManagedWorktreeLeaseState::Ready
                    | crate::protocol::ManagedWorktreeLeaseState::Retained
                    | crate::protocol::ManagedWorktreeLeaseState::CleanupBlocked
                    | crate::protocol::ManagedWorktreeLeaseState::Removed
            ) && (!lease.session_holders.is_empty() || !lease.record_holders.is_empty()))
            || (lease.state == crate::protocol::ManagedWorktreeLeaseState::InUse
                && lease.session_holders.is_empty() && lease.record_holders.is_empty())
            || (matches!(
                lease.state,
                crate::protocol::ManagedWorktreeLeaseState::CleanupBlocked
                    | crate::protocol::ManagedWorktreeLeaseState::RecoveryRequired
            ) != lease.cleanup_failure.is_some())
        {
            return Err(invalid_data("durable managed worktree record is invalid"));
        }
    }
    Ok(())
}

fn valid_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn require_utf8_state_path(path: &OpaqueHostPath) -> io::Result<&str> {
    path.as_utf8().ok_or_else(|| io::Error::new(
        io::ErrorKind::Unsupported,
        StateLoadRefusalError::PathSemanticsUnsupported,
    ))
}

fn is_authoritative_state_error(error: &io::Error) -> bool {
    state_load_refusal(error).is_some()
}

pub(crate) fn state_load_refusal(error: &io::Error) -> Option<StateLoadRefusal> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<StateLoadRefusalError>())
        .map(StateLoadRefusalError::refusal)
}

#[cfg(test)]
pub(crate) fn save(
    path: Option<&Path>,
    node_id: &NodeId,
    workspaces: &BTreeMap<WorkspaceId, String>,
    records: &[ManagedSessionRecord],
) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("refusing to persist too many workspaces"));
    }
    if records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("refusing to persist too many session records"));
    }
    let mut persisted_records = Vec::with_capacity(records.len());
    let mut provider_sessions = BTreeSet::new();
    for record in records {
        let mut persisted = record.clone();
        persisted.active_session = None;
        if let Some(identity) = &mut persisted.provider_session {
            if !provider_sessions.insert(provider_session_semantic_key(
                &persisted.provider,
                identity,
            )) {
                return Err(invalid_data(
                    "refusing to persist duplicate provider sessions",
                ));
            }
            identity.transcript_path = None;
        }
        persisted.last_error = persisted
            .last_error
            .as_deref()
            .map(sanitized_record_error_summary);
        validate_record(&persisted)?;
        persisted.state = if persisted.provider_session.is_some() {
            ManagedSessionState::Dormant
        } else {
            ManagedSessionState::Unavailable
        };
        persisted_records.push(persisted);
    }
    let state = PersistedNodeStateV3 {
        version: NODE_STATE_SCHEMA_V3,
        node_id: node_id.clone(),
        workspaces: workspaces
            .iter()
            .map(|(workspace_id, canonical_root)| {
                Ok(PersistedWorkspaceV2 {
                    workspace_id: workspace_id.clone(),
                    canonical_root: OpaqueHostPath::utf8(canonical_root.clone()).map_err(|error| {
                        invalid_data(format!("refusing to persist an invalid workspace path: {error}"))
                    })?,
                })
            })
            .collect::<io::Result<_>>()?,
        session_records: persisted_records
            .into_iter()
                .map(persisted_v3_record)
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| invalid_data(format!("durable state encoding failed: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("refusing to persist oversized durable state"));
    }
    let previous_primary_valid = if !path.exists() {
        true
    } else {
        match load_one(path, node_id) {
            Ok(_) => true,
            Err(error) if is_authoritative_state_error(&error) => return Err(error),
            Err(_) => false,
        }
    };
    atomic_write(path, &bytes, previous_primary_valid)
}

#[cfg(test)]
pub(crate) fn save_v4(
    path: Option<&Path>,
    node_id: &NodeId,
    workspaces: &BTreeMap<WorkspaceId, String>,
    records: &[ManagedSessionRecord],
    managed_worktrees: &[ManagedWorktreeLeaseRecord],
    managed_worktree_tombstones: &[ManagedWorktreeLeaseRecord],
) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("refusing to persist too many workspaces"));
    }
    if records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("refusing to persist too many session records"));
    }
    validate_managed_worktree_records(managed_worktrees, false)?;
    validate_managed_worktree_records(managed_worktree_tombstones, true)?;
    let mut persisted_records = Vec::with_capacity(records.len());
    let mut provider_sessions = BTreeSet::new();
    for record in records {
        let mut persisted = record.clone();
        persisted.active_session = None;
        if let Some(identity) = &mut persisted.provider_session {
            if !provider_sessions.insert(provider_session_semantic_key(
                &persisted.provider,
                identity,
            )) {
                return Err(invalid_data("refusing to persist duplicate provider sessions"));
            }
            identity.transcript_path = None;
        }
        persisted.last_error = persisted.last_error.as_deref().map(sanitized_record_error_summary);
        validate_record(&persisted)?;
        persisted.state = if persisted.provider_session.is_some() {
            ManagedSessionState::Dormant
        } else {
            ManagedSessionState::Unavailable
        };
        persisted_records.push(persisted_v3_record(persisted));
    }
    let state = PersistedNodeStateV4 {
        version: NODE_STATE_SCHEMA_V4,
        node_id: node_id.clone(),
        workspaces: workspaces.iter().map(|(workspace_id, canonical_root)| {
            Ok(PersistedWorkspaceV2 {
                workspace_id: workspace_id.clone(),
                canonical_root: OpaqueHostPath::utf8(canonical_root.clone()).map_err(|error| {
                    invalid_data(format!("refusing to persist an invalid workspace path: {error}"))
                })?,
            })
        }).collect::<io::Result<_>>()?,
        session_records: persisted_records,
        managed_worktrees: managed_worktrees.to_vec(),
        managed_worktree_tombstones: managed_worktree_tombstones.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| invalid_data(format!("durable state encoding failed: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("refusing to persist oversized durable state"));
    }
    let previous_primary_valid = if !path.exists() {
        true
    } else {
        match load_one(path, node_id) {
            Ok(_) => true,
            Err(error) if is_authoritative_state_error(&error) => return Err(error),
            Err(_) => false,
        }
    };
    atomic_write(path, &bytes, previous_primary_valid)
}

#[cfg(test)]
pub(crate) fn save_v5(
    path: Option<&Path>,
    node_id: &NodeId,
    workspaces: &BTreeMap<WorkspaceId, String>,
    records: &[ManagedSessionRecord],
    managed_worktrees: &[ManagedWorktreeLeaseRecord],
    managed_worktree_tombstones: &[ManagedWorktreeLeaseRecord],
) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("refusing to persist too many workspaces"));
    }
    if records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("refusing to persist too many session records"));
    }
    validate_managed_worktree_records(managed_worktrees, false)?;
    validate_managed_worktree_records(managed_worktree_tombstones, true)?;
    let mut persisted_records = Vec::with_capacity(records.len());
    let mut provider_sessions = BTreeSet::new();
    for record in records {
        let mut persisted = record.clone();
        persisted.active_session = None;
        if let Some(identity) = &mut persisted.provider_session {
            if !provider_sessions.insert(provider_session_semantic_key(
                &persisted.provider,
                identity,
            )) {
                return Err(invalid_data("refusing to persist duplicate provider sessions"));
            }
            identity.transcript_path = None;
        }
        persisted.last_error = persisted.last_error.as_deref().map(sanitized_record_error_summary);
        validate_record(&persisted)?;
        persisted.state = if persisted.provider_session.is_some() {
            ManagedSessionState::Dormant
        } else {
            ManagedSessionState::Unavailable
        };
        persisted_records.push(persisted_v5_record(persisted));
    }
    let state = PersistedNodeStateV5 {
        version: NODE_STATE_SCHEMA_V5,
        node_id: node_id.clone(),
        workspaces: workspaces.iter().map(|(workspace_id, canonical_root)| {
            Ok(PersistedWorkspaceV2 {
                workspace_id: workspace_id.clone(),
                canonical_root: OpaqueHostPath::utf8(canonical_root.clone()).map_err(|error| {
                    invalid_data(format!("refusing to persist an invalid workspace path: {error}"))
                })?,
            })
        }).collect::<io::Result<_>>()?,
        session_records: persisted_records,
        managed_worktrees: managed_worktrees.to_vec(),
        managed_worktree_tombstones: managed_worktree_tombstones.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| invalid_data(format!("durable state encoding failed: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("refusing to persist oversized durable state"));
    }
    let previous_primary_valid = if !path.exists() {
        true
    } else {
        match load_one(path, node_id) {
            Ok(_) => true,
            Err(error) if is_authoritative_state_error(&error) => return Err(error),
            Err(_) => false,
        }
    };
    atomic_write(path, &bytes, previous_primary_valid)
}

pub(crate) fn save_v6(
    path: Option<&Path>,
    node_id: &NodeId,
    workspaces: &BTreeMap<WorkspaceId, String>,
    records: &[ManagedSessionRecord],
    managed_worktrees: &[ManagedWorktreeLeaseRecord],
    managed_worktree_tombstones: &[ManagedWorktreeLeaseRecord],
    materializations: &[MaterializationOwnershipRecord],
) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("refusing to persist too many workspaces"));
    }
    if records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("refusing to persist too many session records"));
    }
    validate_managed_worktree_records(managed_worktrees, false)?;
    validate_managed_worktree_records(managed_worktree_tombstones, true)?;
    validate_managed_worktree_identity_sets(
        managed_worktrees,
        managed_worktree_tombstones,
    )?;
    validate_materialization_records(
        materializations,
        records,
        managed_worktrees,
        managed_worktree_tombstones,
    )?;
    let mut persisted_records = Vec::with_capacity(records.len());
    let mut provider_sessions = BTreeSet::new();
    for record in records {
        let mut persisted = record.clone();
        persisted.active_session = None;
        if let Some(identity) = &mut persisted.provider_session {
            if !provider_sessions.insert(provider_session_semantic_key(&persisted.provider, identity)) {
                return Err(invalid_data("refusing to persist duplicate provider sessions"));
            }
            identity.transcript_path = None;
        }
        persisted.last_error = persisted.last_error.as_deref().map(sanitized_record_error_summary);
        validate_record(&persisted)?;
        persisted.state = if persisted.provider_session.is_some() {
            ManagedSessionState::Dormant
        } else {
            ManagedSessionState::Unavailable
        };
        persisted_records.push(persisted_v5_record(persisted));
    }
    let state = PersistedNodeStateV6 {
        version: NODE_STATE_SCHEMA_V6,
        node_id: node_id.clone(),
        workspaces: workspaces.iter().map(|(workspace_id, canonical_root)| {
            Ok(PersistedWorkspaceV2 {
                workspace_id: workspace_id.clone(),
                canonical_root: OpaqueHostPath::utf8(canonical_root.clone()).map_err(|error| {
                    invalid_data(format!("refusing to persist an invalid workspace path: {error}"))
                })?,
            })
        }).collect::<io::Result<_>>()?,
        session_records: persisted_records,
        managed_worktrees: managed_worktrees.to_vec(),
        managed_worktree_tombstones: managed_worktree_tombstones.to_vec(),
        materializations: materializations.iter().map(|record| {
            Ok(PersistedMaterializationRecordV6 {
                materialization_id: record.id().clone(),
                environment_profile: record.environment_profile().clone(),
                owner: record.owner().clone(),
                managed_lease_id: record.managed_lease_id().cloned(),
                state: record.state(),
                root: path_to_opaque(record.root())?,
                provider_home: path_to_opaque(record.provider_home())?,
                declared_paths: record.declared_paths().to_vec(),
                created_at_unix_ms: record.created_at_unix_ms(),
                updated_at_unix_ms: record.updated_at_unix_ms(),
            })
        }).collect::<io::Result<Vec<_>>>()?,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| invalid_data(format!("durable state encoding failed: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("refusing to persist oversized durable state"));
    }
    let previous_primary_valid = if !path.exists() {
        true
    } else {
        match load_one(path, node_id) {
            Ok(_) => true,
            Err(error) if is_authoritative_state_error(&error) => return Err(error),
            Err(_) => false,
        }
    };
    atomic_write(path, &bytes, previous_primary_valid)
}

#[cfg(test)]
fn persisted_v3_record(record: ManagedSessionRecord) -> PersistedManagedSessionRecordV3 {
    PersistedManagedSessionRecordV3 {
        record_id: record.record_id,
        display_name: record.display_name,
        provider: record.provider,
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id,
        canonical_root: record.canonical_root,
        provider_session: record.provider_session,
        active_session: record.active_session,
        created_at_unix_ms: record.created_at_unix_ms,
        updated_at_unix_ms: record.updated_at_unix_ms,
        last_error: record.last_error,
    }
}

fn persisted_v5_record(record: ManagedSessionRecord) -> PersistedManagedSessionRecordV5 {
    PersistedManagedSessionRecordV5 {
        record_id: record.record_id,
        display_name: record.display_name,
        provider: record.provider,
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id,
        canonical_root: record.canonical_root,
        provider_session: record.provider_session,
        active_session: record.active_session,
        environment_profile: record.environment_profile,
        created_at_unix_ms: record.created_at_unix_ms,
        updated_at_unix_ms: record.updated_at_unix_ms,
        last_error: record.last_error,
    }
}

pub(crate) fn validate_display_name(display_name: &str) -> io::Result<()> {
    if display_name.trim().is_empty()
        || display_name.len() > MAX_SESSION_DISPLAY_NAME_BYTES
        || display_name.chars().any(char::is_control)
    {
        return Err(invalid_data(format!(
            "session display name must contain 1..={MAX_SESSION_DISPLAY_NAME_BYTES} bytes and no control characters",
        )));
    }
    Ok(())
}

fn validate_record(record: &ManagedSessionRecord) -> io::Result<()> {
    validate_display_name(&record.display_name)?;
    validate_root(require_utf8_state_path(&record.canonical_root)?)?;
    if record.created_at_unix_ms > record.updated_at_unix_ms {
        return Err(invalid_data("session record timestamps are inconsistent"));
    }
    if let Some(identity) = &record.provider_session {
        identity
            .validate()
            .map_err(|error| invalid_data(format!("provider session identity is invalid: {error}")))?;
    }
    if record
        .last_error
        .as_ref()
        .is_some_and(|message| {
            message.len() > MAX_NODE_TEXT_BYTES || message.chars().any(|character| character == '\0')
        })
    {
        return Err(invalid_data("session record error is invalid"));
    }
    Ok(())
}

pub(crate) fn sanitized_record_error_summary(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "workspace-unavailable"
            | "provider-session-scope-conflict"
            | "provider-session-live-conflict"
            | "managed-session-capacity"
            | "provider-session-identity-allocation-failed"
            | "provider-resume-rejected"
            | "provider-authentication-required"
            | "provider-readiness-failed"
            | "provider-process-exited"
            | "provider-resume-failed"
            | "durable-state-commit-failed"
            | "provider-runtime-failed"
            | "environment-profile-unavailable"
    ) {
        return normalized;
    }
    if normalized.contains("workspace is unavailable") {
        "workspace-unavailable".to_owned()
    } else if normalized.contains("another workspace or transport") {
        "provider-session-scope-conflict".to_owned()
    } else if normalized.contains("conflicts with another live managed record") {
        "provider-session-live-conflict".to_owned()
    } else if normalized.contains("managed session capacity") {
        "managed-session-capacity".to_owned()
    } else if normalized.contains("allocate a record") {
        "provider-session-identity-allocation-failed".to_owned()
    } else if normalized.contains("resume command was rejected") {
        "provider-resume-rejected".to_owned()
    } else if normalized.contains("authentication") || normalized.contains("sign in") {
        "provider-authentication-required".to_owned()
    } else if normalized.contains("readiness") {
        "provider-readiness-failed".to_owned()
    } else if normalized.contains("exited with code")
        || normalized.contains("no row for root pid")
    {
        "provider-process-exited".to_owned()
    } else if normalized.contains("resume") {
        "provider-resume-failed".to_owned()
    } else if normalized.contains("durable state") || normalized.contains("persistence") {
        "durable-state-commit-failed".to_owned()
    } else {
        "provider-runtime-failed".to_owned()
    }
}

fn validate_root(root: &str) -> io::Result<()> {
    if root.is_empty()
        || root.len() > MAX_WORKSPACE_ROOT_BYTES
        || root.chars().any(char::is_control)
        || !Path::new(root).is_absolute()
    {
        return Err(invalid_data("durable workspace root is invalid"));
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AtomicWriteFault {
    None,
    BeforePrimaryCommit,
    BeforeBackupRotation,
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    previous_primary_valid: bool,
) -> io::Result<Option<String>> {
    atomic_write_with_policy(
        path,
        bytes,
        AtomicWriteFault::None,
        previous_primary_valid,
    )
}

#[cfg(test)]
fn atomic_write_with_fault(
    path: &Path,
    bytes: &[u8],
    fault: AtomicWriteFault,
) -> io::Result<Option<String>> {
    atomic_write_with_policy(path, bytes, fault, true)
}

fn atomic_write_with_policy(
    path: &Path,
    bytes: &[u8],
    fault: AtomicWriteFault,
    previous_primary_valid: bool,
) -> io::Result<Option<String>> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "durable state path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    secure_state_directory(parent)?;
    let (temporary, mut file) = create_unique_sibling_file(path, "tmp")?;
    let backup = sibling_path(path, "bak");
    let prepared = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = prepared {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let pending_backup = sibling_path(path, "pending-backup");
    if pending_backup.exists() {
        let pending_result = if backup.exists() {
            rotate_backup_after_commit(&backup, &pending_backup, AtomicWriteFault::None)
        } else {
            fs::rename(&pending_backup, &backup)
        };
        if let Err(error) = pending_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    let displaced = if path.exists() {
        let displaced = if !previous_primary_valid {
            unique_sibling_path(path, "corrupt")?
        } else if backup.exists() {
            pending_backup.clone()
        } else {
            backup.clone()
        };
        if let Err(error) = fs::rename(path, &displaced) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Some(displaced)
    } else {
        None
    };
    let result = (|| {
        if fault == AtomicWriteFault::BeforePrimaryCommit {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "injected failure before durable primary commit",
            ));
        }
        fs::rename(&temporary, path)?;
        sync_parent_directory(path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        if let Some(displaced) = displaced.as_deref() {
            if !path.exists() {
                fs::rename(displaced, path).map_err(|rollback_error| {
                    io::Error::new(
                        rollback_error.kind(),
                        format!(
                            "durable primary commit failed ({error}); restoring the prior primary also failed: {rollback_error}"
                        ),
                    )
                })?;
            }
        }
        return Err(error);
    }

    let backup_warning = if let Some(displaced) = displaced.as_deref() {
        if displaced == pending_backup {
            rotate_backup_after_commit(&backup, displaced, fault)
                .err()
                .map(|_| BACKUP_ROTATION_WARNING.to_owned())
        } else if displaced != backup {
            Some(CORRUPT_PRIMARY_PRESERVED_WARNING.to_owned())
        } else {
            None
        }
    } else {
        None
    };
    Ok(backup_warning)
}

fn rotate_backup_after_commit(
    backup: &Path,
    previous_primary: &Path,
    fault: AtomicWriteFault,
) -> io::Result<()> {
    if fault == AtomicWriteFault::BeforeBackupRotation {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "injected failure before durable backup rotation",
        ));
    }
    let preserved_backup = unique_sibling_path(backup, "preserved")?;
    replace_file(backup, previous_primary, &preserved_backup)?;
    let _ = fs::remove_file(preserved_backup);
    Ok(())
}

fn replace_file(replaced: &Path, replacement: &Path, preserved: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "ReplaceFileW"]
        fn replace_file_w(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    let replaced = replaced
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let preserved = preserved
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: all paths are NUL-terminated UTF-16 buffers that remain alive
    // for the call. The reserved pointers are null as required by Win32.
    let replaced = unsafe {
        replace_file_w(
            replaced.as_ptr(),
            replacement.as_ptr(),
            preserved.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
    }
    #[cfg(unix)]
    {
        fs::rename(replaced, preserved)?;
        match fs::rename(replacement, replaced) {
            Ok(()) => sync_parent_directory(replaced),
            Err(error) => {
                let rollback = fs::rename(preserved, replaced);
                match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(io::Error::new(
                        rollback_error.kind(),
                        format!(
                            "durable backup rotation failed ({error}); restoring the prior backup also failed: {rollback_error}"
                        ),
                    )),
                }
            }
        }
    }
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "durable state path has no parent")
    })?;
    File::open(parent)?.sync_all()
}

fn create_unique_sibling_file(path: &Path, suffix: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..32 {
        let candidate = unique_sibling_candidate(path, suffix);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique durable state temporary file",
    ))
}

fn unique_sibling_path(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let candidate = unique_sibling_candidate(path, suffix);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique durable state sibling path",
    ))
}

fn unique_sibling_candidate(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state-v1.json");
    let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{name}.{suffix}-{}-{sequence}",
        std::process::id(),
    ))
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state-v1.json");
    path.with_file_name(format!(".{name}.{suffix}"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SessionMode, SessionRecordId};
    use gate4agent_types::{ProviderSessionIdentity, ProviderSessionKey};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn temp_path(test: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("gate4agent-node-state-{}-{unique}", std::process::id()))
            .join(format!("{test}.json"))
    }

    fn fixture(path: &Path) -> (NodeId, BTreeMap<WorkspaceId, String>, ManagedSessionRecord) {
        let node_id = NodeId::new("node-a").unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let root = path.parent().unwrap().to_string_lossy().into_owned();
        let workspaces = BTreeMap::from([(workspace_id.clone(), root.clone())]);
        let record = ManagedSessionRecord {
            record_id: SessionRecordId::new("session-1").unwrap(),
            display_name: "Claude release check".to_owned(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Live,
            workspace_id,
            canonical_root: OpaqueHostPath::utf8(root).unwrap(),
            provider_session: Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "provider-session-1".to_owned(),
                transcript_path: Some(r"C:\private\provider-transcript.jsonl".to_owned()),
            }),
            active_session: None,
            environment_profile: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 11,
            last_error: None,
        };
        (node_id, workspaces, record)
    }

    fn managed_lease(path: &Path, id: &str, workspace: &str, branch: &str) -> ManagedWorktreeLeaseRecord {
        ManagedWorktreeLeaseRecord {
            lease_id: crate::protocol::ManagedWorktreeLeaseId::new(id).unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new(workspace).unwrap(),
            profile_id: crate::protocol::WorktreeProfileId::new("default").unwrap(),
            profile_revision: crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
            target_root: path.to_string_lossy().into_owned(),
            branch: branch.to_owned(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            expected_head: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            retention: crate::protocol::ManagedWorktreeRetention::RemoveWhenReleased,
            state: crate::protocol::ManagedWorktreeLeaseState::Ready,
            session_holders: Vec::new(),
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 11,
        }
    }

    fn materialization_ownership(
        path: &Path,
        id: &str,
        owner: MaterializationOwner,
        managed_lease_id: Option<crate::protocol::ManagedWorktreeLeaseId>,
    ) -> MaterializationOwnershipRecord {
        let root = path
            .parent()
            .unwrap()
            .join("session-environments")
            .join(id);
        MaterializationOwnershipRecord::from_persisted(
            MaterializationId::new(id).unwrap(),
            ResolvedEnvironmentProfileReceipt {
                profile_id: crate::protocol::SpawnEnvironmentProfileId::new("local-claude")
                    .unwrap(),
                profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new("r1")
                    .unwrap(),
            },
            owner,
            managed_lease_id,
            MaterializationState::Ready,
            root.clone(),
            root.join("home"),
            Vec::new(),
            10,
            11,
        )
        .unwrap()
    }

    fn persisted_materialization(
        record: &MaterializationOwnershipRecord,
    ) -> PersistedMaterializationRecordV6 {
        PersistedMaterializationRecordV6 {
            materialization_id: record.id().clone(),
            environment_profile: record.environment_profile().clone(),
            owner: record.owner().clone(),
            managed_lease_id: record.managed_lease_id().cloned(),
            state: record.state(),
            root: path_to_opaque(record.root()).unwrap(),
            provider_home: path_to_opaque(record.provider_home()).unwrap(),
            declared_paths: record.declared_paths().to_vec(),
            created_at_unix_ms: record.created_at_unix_ms(),
            updated_at_unix_ms: record.updated_at_unix_ms(),
        }
    }

    #[test]
    fn v4_roundtrip_preserves_host_only_managed_lease_identity() {
        let path = temp_path("v4-managed-roundtrip");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, record) = fixture(&path);
        let lease = managed_lease(
            &path.parent().unwrap().join("managed-a"),
            "mw-a",
            "managed-a",
            "gate4agent/mw-a",
        );
        save_v4(
            Some(&path),
            &node_id,
            &workspaces,
            &[record],
            &[lease.clone()],
            &[],
        ).unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(
            &std::fs::read(&path).unwrap(),
        ).unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V4);
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.managed_worktrees, vec![lease]);
        assert!(loaded.managed_worktree_tombstones.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v5_roundtrip_preserves_only_opaque_environment_profile_identity() {
        let path = temp_path("v5-environment-profile-roundtrip");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, mut record) = fixture(&path);
        let environment_profile = ResolvedEnvironmentProfileReceipt {
            profile_id: crate::protocol::SpawnEnvironmentProfileId::new("local-claude").unwrap(),
            profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new(
                "local-claude-r1",
            )
            .unwrap(),
        };
        record.environment_profile = Some(environment_profile.clone());

        save_v5(
            Some(&path),
            &node_id,
            &workspaces,
            &[record],
            &[],
            &[],
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V5);
        let serialized = String::from_utf8(bytes).unwrap();
        assert!(serialized.contains("local-claude"));
        assert!(!serialized.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!serialized.contains("secret"));

        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(
            loaded.records[0].environment_profile.as_ref(),
            Some(&environment_profile),
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v4_to_v5_migration_defaults_environment_profile_to_none() {
        let path = temp_path("v4-to-v5-environment-profile");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, record) = fixture(&path);
        save_v4(Some(&path), &node_id, &workspaces, &[record], &[], &[]).unwrap();

        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert!(loaded.records[0].environment_profile.is_none());
        save_v5(
            Some(&path),
            &node_id,
            &workspaces,
            &loaded.records,
            &[],
            &[],
        )
        .unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(
            &std::fs::read(&path).unwrap(),
        )
        .unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V5);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v5_to_v6_migration_starts_with_empty_materialization_registry() {
        let path = temp_path("v5-to-v6-materializations");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, record) = fixture(&path);
        save_v5(Some(&path), &node_id, &workspaces, &[record], &[], &[]).unwrap();
        let loaded = load(Some(&path), &node_id).unwrap();
        assert!(loaded.materializations.is_empty());
        save_v6(
            Some(&path),
            &node_id,
            &workspaces,
            &loaded.records,
            &[],
            &[],
            &loaded.materializations,
        ).unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V6);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_roundtrip_persists_ownership_without_secret_reference_or_value() {
        let path = temp_path("v6-materialization-roundtrip");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, _) = fixture(&path);
        let materialization_root = path.parent().unwrap().join("session-environments").join("m-1");
        let ownership = MaterializationOwnershipRecord::from_persisted(
            MaterializationId::new("m-1").unwrap(),
            ResolvedEnvironmentProfileReceipt {
                profile_id: crate::protocol::SpawnEnvironmentProfileId::new("local-claude").unwrap(),
                profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new("r1").unwrap(),
            },
            MaterializationOwner::Session {
                incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([3; crate::protocol::NODE_INCARNATION_ID_BYTES]),
                instance_id: gate4agent_types::AgentInstanceId(9),
                generation: gate4agent_types::SessionGeneration(2),
            },
            None,
            MaterializationState::Ready,
            materialization_root.clone(),
            materialization_root.join("home"),
            vec![MaterializedPathDeclaration {
                class: crate::session_environment::NodeSessionPathClass::Config,
                relative_path: PathBuf::from("provider/settings.json"),
                kind: crate::session_environment::MaterializedPathKind::Generated,
            }],
            10,
            11,
        ).unwrap();
        save_v6(Some(&path), &node_id, &workspaces, &[], &[], &[], &[ownership.clone()]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let serialized = String::from_utf8(bytes).unwrap();
        assert!(!serialized.contains("fixture-secret-reference"));
        assert!(!serialized.contains("fixture-secret-value"));
        let loaded = load(Some(&path), &node_id).unwrap();
        assert!(loaded.materializations == vec![ownership]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_save_rejects_unknown_materialization_managed_lease() {
        let path = temp_path("v6-unknown-materialization-lease");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let ownership = materialization_ownership(
            &path,
            "m-unknown",
            MaterializationOwner::Session {
                incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([
                    3;
                    crate::protocol::NODE_INCARNATION_ID_BYTES
                ]),
                instance_id: gate4agent_types::AgentInstanceId(9),
                generation: gate4agent_types::SessionGeneration(2),
            },
            Some(crate::protocol::ManagedWorktreeLeaseId::new("mw-missing").unwrap()),
        );

        let error = save_v6(
            Some(&path),
            &node_id,
            &BTreeMap::new(),
            &[],
            &[],
            &[],
            &[ownership],
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_decode_rejects_tombstoned_materialization_managed_lease() {
        let path = temp_path("v6-tombstoned-materialization-lease");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let mut tombstone = managed_lease(
            &path.parent().unwrap().join("managed-a"),
            "mw-a",
            "managed-a",
            "gate4agent/mw-a",
        );
        tombstone.state = crate::protocol::ManagedWorktreeLeaseState::Removed;
        let ownership = materialization_ownership(
            &path,
            "m-tombstone",
            MaterializationOwner::Session {
                incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([
                    3;
                    crate::protocol::NODE_INCARNATION_ID_BYTES
                ]),
                instance_id: gate4agent_types::AgentInstanceId(9),
                generation: gate4agent_types::SessionGeneration(2),
            },
            Some(tombstone.lease_id.clone()),
        );
        let state = PersistedNodeStateV6 {
            version: NODE_STATE_SCHEMA_V6,
            node_id: node_id.clone(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            managed_worktree_tombstones: vec![tombstone],
            materializations: vec![persisted_materialization(&ownership)],
        };
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        assert_eq!(
            load(Some(&path), &node_id).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_save_rejects_record_materialization_for_foreign_lease_workspace() {
        let path = temp_path("v6-record-foreign-lease-workspace");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, record) = fixture(&path);
        let lease = managed_lease(
            &path.parent().unwrap().join("managed-a"),
            "mw-a",
            "managed-a",
            "gate4agent/mw-a",
        );
        let ownership = materialization_ownership(
            &path,
            "m-foreign",
            MaterializationOwner::Record {
                record_id: record.record_id.clone(),
            },
            Some(lease.lease_id.clone()),
        );

        let error = save_v6(
            Some(&path),
            &node_id,
            &workspaces,
            &[record],
            &[lease],
            &[],
            &[ownership],
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_roundtrip_accepts_session_materialization_for_active_lease() {
        let path = temp_path("v6-session-active-materialization-lease");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let lease = managed_lease(
            &path.parent().unwrap().join("managed-a"),
            "mw-a",
            "managed-a",
            "gate4agent/mw-a",
        );
        let ownership = materialization_ownership(
            &path,
            "m-active",
            MaterializationOwner::Session {
                incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([
                    3;
                    crate::protocol::NODE_INCARNATION_ID_BYTES
                ]),
                instance_id: gate4agent_types::AgentInstanceId(9),
                generation: gate4agent_types::SessionGeneration(2),
            },
            Some(lease.lease_id.clone()),
        );

        save_v6(
            Some(&path),
            &node_id,
            &BTreeMap::new(),
            &[],
            &[lease.clone()],
            &[],
            &[ownership.clone()],
        )
        .unwrap();
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.managed_worktrees, vec![lease]);
        assert!(loaded.materializations == vec![ownership]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_rejects_declared_path_prefix_collisions() {
        let path = temp_path("v6-prefix-collision");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let materialization_root = path.parent().unwrap().join("session-environments").join("m-1");
        let state = PersistedNodeStateV6 {
            version: NODE_STATE_SCHEMA_V6,
            node_id: node_id.clone(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            managed_worktree_tombstones: Vec::new(),
            materializations: vec![PersistedMaterializationRecordV6 {
                materialization_id: MaterializationId::new("m-1").unwrap(),
                environment_profile: ResolvedEnvironmentProfileReceipt {
                    profile_id: crate::protocol::SpawnEnvironmentProfileId::new("local-claude").unwrap(),
                    profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new("r1").unwrap(),
                },
                owner: MaterializationOwner::Session {
                    incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([3; crate::protocol::NODE_INCARNATION_ID_BYTES]),
                    instance_id: gate4agent_types::AgentInstanceId(9),
                    generation: gate4agent_types::SessionGeneration(2),
                },
                managed_lease_id: None,
                state: MaterializationState::Ready,
                root: path_to_opaque(&materialization_root).unwrap(),
                provider_home: path_to_opaque(&materialization_root.join("home")).unwrap(),
                declared_paths: vec![
                    MaterializedPathDeclaration {
                        class: crate::session_environment::NodeSessionPathClass::Config,
                        relative_path: PathBuf::from("provider"),
                        kind: crate::session_environment::MaterializedPathKind::Generated,
                    },
                    MaterializedPathDeclaration {
                        class: crate::session_environment::NodeSessionPathClass::Config,
                        relative_path: PathBuf::from("provider/settings.json"),
                        kind: crate::session_environment::MaterializedPathKind::Generated,
                    },
                ],
                created_at_unix_ms: 10,
                updated_at_unix_ms: 11,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(load(Some(&path), &node_id).unwrap_err().kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v6_rejects_case_ambiguous_materialization_ids() {
        let path = temp_path("v6-case-ambiguous-materialization-id");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let first_root = path.parent().unwrap().join("session-environments").join("a");
        let second_root = path.parent().unwrap().join("session-environments").join("b");
        let first = PersistedMaterializationRecordV6 {
            materialization_id: MaterializationId::new("a").unwrap(),
            environment_profile: ResolvedEnvironmentProfileReceipt {
                profile_id: crate::protocol::SpawnEnvironmentProfileId::new("local-claude").unwrap(),
                profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new("r1").unwrap(),
            },
            owner: MaterializationOwner::Session {
                incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([3; crate::protocol::NODE_INCARNATION_ID_BYTES]),
                instance_id: gate4agent_types::AgentInstanceId(9),
                generation: gate4agent_types::SessionGeneration(2),
            },
            managed_lease_id: None,
            state: MaterializationState::Ready,
            root: path_to_opaque(&first_root).unwrap(),
            provider_home: path_to_opaque(&first_root.join("home")).unwrap(),
            declared_paths: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 11,
        };
        let mut second = first.clone();
        second.materialization_id = MaterializationId::new("b").unwrap();
        second.owner = MaterializationOwner::Session {
            incarnation_id: crate::protocol::NodeIncarnationId::from_bytes([3; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            instance_id: gate4agent_types::AgentInstanceId(10),
            generation: gate4agent_types::SessionGeneration(2),
        };
        second.root = path_to_opaque(&second_root).unwrap();
        second.provider_home = path_to_opaque(&second_root.join("home")).unwrap();
        let state = PersistedNodeStateV6 {
            version: NODE_STATE_SCHEMA_V6,
            node_id: node_id.clone(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            managed_worktree_tombstones: Vec::new(),
            materializations: vec![first, second],
        };
        let mut value = serde_json::to_value(state).unwrap();
        value["materializations"][1]["materialization_id"] = serde_json::Value::String("A".to_owned());
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(load(Some(&path), &node_id).unwrap_err().kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_v4_duplicate_branch_is_rejected_without_startup_panic() {
        let path = temp_path("v4-managed-duplicate-branch");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (node_id, workspaces, record) = fixture(&path);
        let first = managed_lease(
            &path.parent().unwrap().join("managed-a"),
            "mw-a",
            "managed-a",
            "gate4agent/shared",
        );
        let second = managed_lease(
            &path.parent().unwrap().join("managed-b"),
            "mw-b",
            "managed-b",
            "gate4agent/shared",
        );
        let state = PersistedNodeStateV4 {
            version: NODE_STATE_SCHEMA_V4,
            node_id: node_id.clone(),
            workspaces: workspaces.into_iter().map(|(workspace_id, canonical_root)| {
                PersistedWorkspaceV2 {
                    workspace_id,
                    canonical_root: OpaqueHostPath::utf8(canonical_root).unwrap(),
                }
            }).collect(),
            session_records: vec![persisted_v3_record(record)],
            managed_worktrees: vec![first, second],
            managed_worktree_tombstones: Vec::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(load(Some(&path), &node_id).unwrap_err().kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn persisted_v1_record(record: ManagedSessionRecord) -> PersistedManagedSessionRecordV1 {
        PersistedManagedSessionRecordV1 {
            record_id: record.record_id,
            display_name: record.display_name,
            provider: LegacyAgentProvider::from_agent_id(&record.provider),
            mode: record.mode,
            state: record.state,
            workspace_id: record.workspace_id,
            canonical_root: record.canonical_root.as_utf8().unwrap().to_owned(),
            provider_session: record.provider_session,
            active_session: record.active_session,
            created_at_unix_ms: record.created_at_unix_ms,
            updated_at_unix_ms: record.updated_at_unix_ms,
            last_error: record.last_error,
        }
    }

    fn persisted_v1_state(
        version: u16,
        node_id: NodeId,
        workspaces: &BTreeMap<WorkspaceId, String>,
        records: Vec<ManagedSessionRecord>,
    ) -> PersistedNodeStateV1 {
        PersistedNodeStateV1 {
            version,
            node_id,
            workspaces: workspaces
                .iter()
                .map(|(workspace_id, canonical_root)| PersistedWorkspaceV1 {
                    workspace_id: workspace_id.clone(),
                    canonical_root: canonical_root.clone(),
                })
                .collect(),
            session_records: records.into_iter().map(persisted_v1_record).collect(),
        }
    }

    fn persisted_v2_state(
        node_id: NodeId,
        workspaces: &BTreeMap<WorkspaceId, String>,
        records: Vec<ManagedSessionRecord>,
    ) -> PersistedNodeStateV2 {
        PersistedNodeStateV2 {
            version: NODE_STATE_SCHEMA_V2,
            node_id,
            workspaces: workspaces
                .iter()
                .map(|(workspace_id, canonical_root)| PersistedWorkspaceV2 {
                    workspace_id: workspace_id.clone(),
                    canonical_root: OpaqueHostPath::utf8(canonical_root.clone()).unwrap(),
                })
                .collect(),
            session_records: records
                .into_iter()
                .map(|record| PersistedManagedSessionRecordV2 {
                    record_id: record.record_id,
                    display_name: record.display_name,
                    provider: LegacyAgentProvider::from_agent_id(&record.provider),
                    mode: record.mode,
                    state: record.state,
                    workspace_id: record.workspace_id,
                    canonical_root: record.canonical_root,
                    provider_session: record.provider_session,
                    active_session: record.active_session,
                    created_at_unix_ms: record.created_at_unix_ms,
                    updated_at_unix_ms: record.updated_at_unix_ms,
                    last_error: record.last_error,
                })
                .collect(),
        }
    }

    #[test]
    fn durable_state_round_trips_without_ephemeral_active_binding() {
        let path = temp_path("round-trip");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.provider_session.as_mut().unwrap().transcript_path =
            Some("C:\\private\\bearer-secret\nprovider-transcript.jsonl".to_owned());
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let persisted = fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<PersistedNodeStateHeader>(&persisted)
                .unwrap()
                .version,
            NODE_STATE_SCHEMA_V3,
        );
        assert!(!persisted.contains("provider-transcript.jsonl"));
        assert!(!persisted.contains(r"C:\private"));
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.workspaces, workspaces);
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].record_id, record.record_id);
        assert_eq!(loaded.records[0].state, ManagedSessionState::Dormant);
        assert_eq!(loaded.records[0].active_session, None);
        assert_eq!(
            loaded.records[0]
                .provider_session
                .as_ref()
                .unwrap()
                .transcript_path,
            None,
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v3_state_round_trips_open_provider_id_exactly() {
        let path = temp_path("v3-open-provider-round-trip");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.provider = agent("qwen-code");
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();

        let persisted: PersistedNodeStateV3 =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.version, NODE_STATE_SCHEMA_V3);
        assert_eq!(persisted.session_records.len(), 1);
        assert_eq!(persisted.session_records[0].provider, agent("qwen-code"));

        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].provider, agent("qwen-code"));
        assert_eq!(loaded.records[0].record_id, record.record_id);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn semantic_provider_session_key_ignores_only_transcript_path() {
        let without_path = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "provider-session-1".to_owned(),
            transcript_path: None,
        };
        let with_path = ProviderSessionIdentity {
            transcript_path: Some(r"C:\private\provider-transcript.jsonl".to_owned()),
            ..without_path.clone()
        };
        assert_eq!(
            provider_session_semantic_key(&agent("claude"), &without_path),
            provider_session_semantic_key(&agent("claude"), &with_path),
        );
        assert_ne!(
            provider_session_semantic_key(&agent("claude"), &without_path),
            provider_session_semantic_key(&agent("codex"), &with_path),
        );
        let conversation = ProviderSessionIdentity {
            key: ProviderSessionKey::ConversationId,
            ..with_path
        };
        assert_ne!(
            provider_session_semantic_key(&agent("claude"), &without_path),
            provider_session_semantic_key(&agent("claude"), &conversation),
        );
    }

    #[test]
    fn legacy_state_transcript_path_is_removed_from_loaded_identity() {
        let path = temp_path("legacy-transcript-path");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.provider_session.as_mut().unwrap().transcript_path =
            Some("C:\\private\\bearer-secret\nprovider-transcript.jsonl".to_owned());
        let legacy = persisted_v1_state(
            NODE_STATE_SCHEMA_V1,
            node_id.clone(),
            &workspaces,
            vec![record],
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(
            loaded.records[0]
                .provider_session
                .as_ref()
                .unwrap()
                .transcript_path,
            None,
        );
        let snapshot = serde_json::to_string(&loaded.records).unwrap();
        assert!(!snapshot.contains("provider-transcript.jsonl"));
        assert!(!snapshot.contains("bearer-secret"));
        assert!(!snapshot.contains(r"C:\private"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v1_state_loads_losslessly_and_next_save_migrates_to_v3() {
        let path = temp_path("v1-migration");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, record) = fixture(&path);
        let legacy = persisted_v1_state(
            NODE_STATE_SCHEMA_V1,
            node_id.clone(),
            &workspaces,
            vec![record.clone()],
        );
        let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &legacy_bytes).unwrap();

        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.workspaces, workspaces);
        assert_eq!(loaded.records[0].record_id, record.record_id);
        assert_eq!(
            loaded.records[0].canonical_root,
            record.canonical_root,
        );
        save(Some(&path), &node_id, &loaded.workspaces, &loaded.records).unwrap();

        let migrated = fs::read(&path).unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(&migrated).unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V3);
        assert_eq!(fs::read(&backup).unwrap(), legacy_bytes);
        let reloaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(reloaded.workspaces, workspaces);
        assert_eq!(reloaded.records, loaded.records);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v2_state_loads_losslessly_and_next_save_migrates_to_v3() {
        let path = temp_path("v2-migration");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, record) = fixture(&path);
        let legacy = persisted_v2_state(
            node_id.clone(),
            &workspaces,
            vec![record.clone()],
        );
        let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &legacy_bytes).unwrap();

        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.workspaces, workspaces);
        assert_eq!(loaded.records[0].provider, agent("claude"));
        assert_eq!(loaded.records[0].record_id, record.record_id);
        save(Some(&path), &node_id, &loaded.workspaces, &loaded.records).unwrap();

        let migrated = fs::read(&path).unwrap();
        let header: PersistedNodeStateHeader = serde_json::from_slice(&migrated).unwrap();
        assert_eq!(header.version, NODE_STATE_SCHEMA_V3);
        assert_eq!(fs::read(&backup).unwrap(), legacy_bytes);
        assert_eq!(load(Some(&path), &node_id).unwrap().records, loaded.records);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_rejects_semantic_provider_session_duplicates_before_commit() {
        let path = temp_path("duplicate-provider-session-save");
        let (node_id, workspaces, record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let original = fs::read(&path).unwrap();
        let mut duplicate = record.clone();
        duplicate.record_id = SessionRecordId::new("session-2").unwrap();
        duplicate.display_name = "duplicate".to_owned();
        duplicate.provider_session.as_mut().unwrap().transcript_path =
            Some(r"D:\another\transcript.jsonl".to_owned());
        let error = save(
            Some(&path),
            &node_id,
            &workspaces,
            &[record, duplicate],
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), original);
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_rejects_semantic_provider_session_duplicates_with_different_paths() {
        let path = temp_path("duplicate-provider-session-load");
        let (node_id, workspaces, record) = fixture(&path);
        let mut duplicate = record.clone();
        duplicate.record_id = SessionRecordId::new("session-2").unwrap();
        duplicate.display_name = "duplicate".to_owned();
        duplicate.provider_session.as_mut().unwrap().transcript_path =
            Some(r"D:\another\transcript.jsonl".to_owned());
        let legacy = persisted_v1_state(
            NODE_STATE_SCHEMA_V1,
            node_id.clone(),
            &workspaces,
            vec![record, duplicate],
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn durable_state_persists_only_allowlisted_error_summaries() {
        let path = temp_path("sanitized-error");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.last_error = Some(
            r"provider failed with bearer-secret-123 at C:\private\session.jsonl".to_owned(),
        );
        save(Some(&path), &node_id, &workspaces, &[record]).unwrap();
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains("bearer-secret-123"));
        assert!(!persisted.contains(r"C:\private"));
        assert!(persisted.contains("provider-runtime-failed"));
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(
            loaded.records[0].last_error.as_deref(),
            Some("provider-runtime-failed"),
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_primary_recovers_valid_backup_without_rewriting_corrupt_bytes() {
        let path = temp_path("recover");
        let (node_id, workspaces, record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let mut renamed = record;
        renamed.display_name = "renamed".to_owned();
        save(Some(&path), &node_id, &workspaces, &[renamed]).unwrap();
        fs::write(&path, b"{corrupt").unwrap();
        let corrupt = fs::read(&path).unwrap();
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records[0].display_name, "Claude release check");
        assert_eq!(
            loaded.warning.as_deref(),
            Some(DURABLE_STATE_RECOVERY_WARNING),
        );
        assert!(!loaded.warning.unwrap().contains(&path.to_string_lossy().into_owned()));
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn newer_primary_schema_refuses_without_falling_back_to_valid_v1_backup() {
        let path = temp_path("newer-primary-schema");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        load_one(&backup, &node_id).unwrap();

        let future = persisted_v1_state(
            NODE_STATE_SCHEMA_V3 + 1,
            node_id.clone(),
            &workspaces,
            vec![record.clone()],
        );
        let future_bytes = serde_json::to_vec_pretty(&future).unwrap();
        fs::write(&path, &future_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::UnsupportedSchema),
        );
        assert_eq!(fs::read(&path).unwrap(), future_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let save_error = save(Some(&path), &node_id, &workspaces, &[record]).unwrap_err();
        assert_eq!(save_error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(fs::read(&path).unwrap(), future_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn newer_pending_schema_refuses_without_skipping_to_older_backup() {
        let path = temp_path("newer-pending-schema");
        let backup = sibling_path(&path, "bak");
        let pending_backup = sibling_path(&path, "pending-backup");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        load_one(&backup, &node_id).unwrap();
        fs::remove_file(&path).unwrap();

        let future = persisted_v1_state(
            NODE_STATE_SCHEMA_V3 + 1,
            node_id.clone(),
            &workspaces,
            vec![record],
        );
        let future_bytes = serde_json::to_vec_pretty(&future).unwrap();
        fs::write(&pending_backup, &future_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::UnsupportedSchema),
        );
        assert_eq!(fs::read(&pending_backup).unwrap(), future_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v2_unix_bytes_primary_refuses_without_fallback_or_rewrite() {
        let path = temp_path("unix-bytes-primary");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let foreign = PersistedNodeStateV2 {
            version: NODE_STATE_SCHEMA_V2,
            node_id: node_id.clone(),
            workspaces: vec![PersistedWorkspaceV2 {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                canonical_root: OpaqueHostPath::unix_bytes(b"/srv/repo".to_vec()).unwrap(),
            }],
            session_records: Vec::new(),
        };
        let foreign_bytes = serde_json::to_vec_pretty(&foreign).unwrap();
        fs::write(&path, &foreign_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::PathSemanticsUnsupported),
        );
        let save_error = save(Some(&path), &node_id, &workspaces, &[record]).unwrap_err();
        assert_eq!(
            state_load_refusal(&save_error),
            Some(StateLoadRefusal::PathSemanticsUnsupported),
        );
        assert_eq!(fs::read(&path).unwrap(), foreign_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v2_unix_bytes_pending_refuses_without_skipping_older_backup() {
        let path = temp_path("unix-bytes-pending");
        let backup = sibling_path(&path, "bak");
        let pending_backup = sibling_path(&path, "pending-backup");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record]).unwrap();
        fs::remove_file(&path).unwrap();
        let foreign = PersistedNodeStateV2 {
            version: NODE_STATE_SCHEMA_V2,
            node_id: node_id.clone(),
            workspaces: vec![PersistedWorkspaceV2 {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                canonical_root: OpaqueHostPath::unix_bytes(b"/srv/repo".to_vec()).unwrap(),
            }],
            session_records: Vec::new(),
        };
        let foreign_bytes = serde_json::to_vec_pretty(&foreign).unwrap();
        fs::write(&pending_backup, &foreign_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::PathSemanticsUnsupported),
        );
        assert_eq!(fs::read(&pending_backup).unwrap(), foreign_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wrong_node_primary_refuses_without_falling_back_or_rewriting() {
        let path = temp_path("wrong-node-primary");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        load_one(&backup, &node_id).unwrap();

        let wrong_node = persisted_v1_state(
            NODE_STATE_SCHEMA_V1,
            NodeId::new("node-b").unwrap(),
            &workspaces,
            vec![record.clone()],
        );
        let wrong_node_bytes = serde_json::to_vec_pretty(&wrong_node).unwrap();
        fs::write(&path, &wrong_node_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::NodeIdentityMismatch),
        );
        let save_error = save(Some(&path), &node_id, &workspaces, &[record]).unwrap_err();
        assert_eq!(save_error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&path).unwrap(), wrong_node_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wrong_node_pending_refuses_without_skipping_to_older_backup() {
        let path = temp_path("wrong-node-pending");
        let backup = sibling_path(&path, "bak");
        let pending_backup = sibling_path(&path, "pending-backup");
        let (node_id, workspaces, mut record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        load_one(&backup, &node_id).unwrap();
        fs::remove_file(&path).unwrap();

        let wrong_node = persisted_v1_state(
            NODE_STATE_SCHEMA_V1,
            NodeId::new("node-b").unwrap(),
            &workspaces,
            vec![record],
        );
        let wrong_node_bytes = serde_json::to_vec_pretty(&wrong_node).unwrap();
        fs::write(&pending_backup, &wrong_node_bytes).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();

        let error = load(Some(&path), &node_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            state_load_refusal(&error),
            Some(StateLoadRefusal::NodeIdentityMismatch),
        );
        assert_eq!(fs::read(&pending_backup).unwrap(), wrong_node_bytes);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn authoritative_refusals_are_typed_not_inferred_from_io_kinds() {
        let generic_permission = io::Error::new(io::ErrorKind::PermissionDenied, "access denied");
        let generic_unsupported = io::Error::new(io::ErrorKind::Unsupported, "operation unsupported");
        assert_eq!(state_load_refusal(&generic_permission), None);
        assert_eq!(state_load_refusal(&generic_unsupported), None);

        let schema = io::Error::new(
            io::ErrorKind::Unsupported,
            StateLoadRefusalError::UnsupportedSchema(NODE_STATE_SCHEMA_V3 + 1),
        );
        let node = io::Error::new(
            io::ErrorKind::PermissionDenied,
            StateLoadRefusalError::NodeIdentityMismatch,
        );
        let path = io::Error::new(
            io::ErrorKind::Unsupported,
            StateLoadRefusalError::PathSemanticsUnsupported,
        );
        assert_eq!(
            state_load_refusal(&schema),
            Some(StateLoadRefusal::UnsupportedSchema),
        );
        assert_eq!(
            state_load_refusal(&node),
            Some(StateLoadRefusal::NodeIdentityMismatch),
        );
        assert_eq!(
            state_load_refusal(&path),
            Some(StateLoadRefusal::PathSemanticsUnsupported),
        );
    }

    #[test]
    fn backup_tracks_the_immediately_previous_generation_across_four_commits() {
        let path = temp_path("backup-generations");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, mut record) = fixture(&path);
        for generation in 1..=4 {
            record.display_name = format!("generation-{generation}");
            record.updated_at_unix_ms = 10 + generation;
            let warning = save(
                Some(&path),
                &node_id,
                &workspaces,
                &[record.clone()],
            )
            .unwrap();
            assert_eq!(warning, None);
            if generation > 1 {
                let loaded_backup = load_one(&backup, &node_id).unwrap();
                assert_eq!(
                    loaded_backup.records[0].display_name,
                    format!("generation-{}", generation - 1),
                );
            }
        }

        fs::write(&path, b"{corrupt").unwrap();
        let recovered = load(Some(&path), &node_id).unwrap();
        assert_eq!(recovered.records[0].display_name, "generation-3");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_backup_rotation_preserves_old_backup_and_pending_previous_primary() {
        let path = temp_path("backup-rotation-failure");
        let backup = sibling_path(&path, "bak");
        let pending_backup = sibling_path(&path, "pending-backup");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.display_name = "generation-1".to_owned();
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        record.display_name = "generation-2".to_owned();
        record.updated_at_unix_ms += 1;
        save(Some(&path), &node_id, &workspaces, &[record]).unwrap();
        let old_backup = fs::read(&backup).unwrap();
        let previous_primary = fs::read(&path).unwrap();

        let warning = atomic_write_with_fault(
            &path,
            &previous_primary,
            AtomicWriteFault::BeforeBackupRotation,
        )
        .unwrap();
        assert_eq!(warning.as_deref(), Some(BACKUP_ROTATION_WARNING));
        assert_eq!(fs::read(&backup).unwrap(), old_backup);
        assert_eq!(fs::read(&pending_backup).unwrap(), previous_primary);
        fs::write(&path, b"{corrupt").unwrap();
        let recovered = load(Some(&path), &node_id).unwrap();
        assert_eq!(recovered.records[0].display_name, "generation-2");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_recovery_commit_preserves_the_valid_backup() {
        let path = temp_path("recover-failed-commit");
        let backup = sibling_path(&path, "bak");
        let (node_id, workspaces, record) = fixture(&path);
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let mut renamed = record;
        renamed.display_name = "renamed".to_owned();
        save(Some(&path), &node_id, &workspaces, &[renamed]).unwrap();
        fs::write(&path, b"{corrupt").unwrap();
        let valid_backup = fs::read(&backup).unwrap();
        let corrupt_primary = fs::read(&path).unwrap();

        let error = atomic_write_with_policy(
            &path,
            br#"{"new":"state"}"#,
            AtomicWriteFault::BeforePrimaryCommit,
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&backup).unwrap(), valid_backup);
        assert_eq!(fs::read(&path).unwrap(), corrupt_primary);
        let loaded = load(Some(&path), &node_id).unwrap();
        assert_eq!(loaded.records[0].display_name, "Claude release check");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn state_path_lock_rejects_a_second_owner_until_drop() {
        let path = temp_path("exclusive-owner");
        let first = StatePathLock::acquire(Some(&path)).unwrap().unwrap();
        let error = match StatePathLock::acquire(Some(&path)) {
            Ok(_) => panic!("a second durable state owner was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        let second = StatePathLock::acquire(Some(&path)).unwrap().unwrap();
        drop(second);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_only_copy_fails_and_is_preserved() {
        let path = temp_path("corrupt-only");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not-json").unwrap();
        let error = load(Some(&path), &NodeId::new("node-a").unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), b"not-json");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
