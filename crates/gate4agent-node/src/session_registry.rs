use crate::protocol::{
    ManagedSessionRecord, ManagedSessionState, NodeId, WorkspaceId,
    MAX_NODE_TEXT_BYTES, MAX_SESSION_DISPLAY_NAME_BYTES, MAX_WORKSPACE_ROOT_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const STATE_VERSION: u16 = 1;
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PERSISTED_WORKSPACES: usize = 256;
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
    provider: crate::protocol::AgentProvider,
    identity: &gate4agent_types::ProviderSessionIdentity,
) -> ProviderSessionSemanticKey {
    let key = match identity.key {
        gate4agent_types::ProviderSessionKey::SessionId => 0,
        gate4agent_types::ProviderSessionKey::ConversationId => 1,
    };
    (provider.agent_id().to_owned(), key, identity.id.clone())
}

pub(crate) fn same_provider_session(
    left_provider: crate::protocol::AgentProvider,
    left: &gate4agent_types::ProviderSessionIdentity,
    right_provider: crate::protocol::AgentProvider,
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
        let lock_path = sibling_path(path, "lock");
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
        Ok(Some(Self {
            file: Some(file),
            path: lock_path,
        }))
    }
}

impl Drop for StatePathLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedNodeState {
    pub workspaces: BTreeMap<WorkspaceId, String>,
    pub records: Vec<ManagedSessionRecord>,
    pub warning: Option<String>,
}

impl LoadedNodeState {
    fn empty() -> Self {
        Self {
            workspaces: BTreeMap::new(),
            records: Vec::new(),
            warning: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNodeState {
    version: u16,
    node_id: NodeId,
    workspaces: Vec<PersistedWorkspace>,
    session_records: Vec<ManagedSessionRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedWorkspace {
    workspace_id: WorkspaceId,
    canonical_root: String,
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
    let state: PersistedNodeState = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("durable state JSON is invalid: {error}")))?;
    if state.version != STATE_VERSION {
        return Err(invalid_data(format!(
            "unsupported durable state version {}",
            state.version,
        )));
    }
    if &state.node_id != expected_node_id {
        return Err(invalid_data(format!(
            "durable state belongs to node '{}' instead of '{}'",
            state.node_id, expected_node_id,
        )));
    }
    if state.workspaces.len() > MAX_PERSISTED_WORKSPACES {
        return Err(invalid_data("durable state contains too many workspaces"));
    }
    if state.session_records.len() > MAX_MANAGED_SESSION_RECORDS {
        return Err(invalid_data("durable state contains too many session records"));
    }

    let mut workspaces = BTreeMap::new();
    let mut roots = BTreeSet::new();
    for workspace in state.workspaces {
        validate_root(&workspace.canonical_root)?;
        if workspaces
            .insert(workspace.workspace_id.clone(), workspace.canonical_root.clone())
            .is_some()
        {
            return Err(invalid_data("durable state contains duplicate workspace IDs"));
        }
        if !roots.insert(workspace.canonical_root.to_lowercase()) {
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
            let key = provider_session_semantic_key(record.provider, identity);
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
        warning: None,
    })
}

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
                persisted.provider,
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
    let state = PersistedNodeState {
        version: STATE_VERSION,
        node_id: node_id.clone(),
        workspaces: workspaces
            .iter()
            .map(|(workspace_id, canonical_root)| PersistedWorkspace {
                workspace_id: workspace_id.clone(),
                canonical_root: canonical_root.clone(),
            })
            .collect(),
        session_records: persisted_records,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| invalid_data(format!("durable state encoding failed: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("refusing to persist oversized durable state"));
    }
    let previous_primary_valid = !path.exists() || load_one(path, node_id).is_ok();
    atomic_write(path, &bytes, previous_primary_valid)
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
    validate_root(&record.canonical_root)?;
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
        fs::rename(&temporary, path)
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

fn create_unique_sibling_file(path: &Path, suffix: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..32 {
        let candidate = unique_sibling_candidate(path, suffix);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
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
    use crate::protocol::{AgentProvider, SessionMode, SessionRecordId};
    use gate4agent_types::{ProviderSessionIdentity, ProviderSessionKey};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

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
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Live,
            workspace_id,
            canonical_root: root,
            provider_session: Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "provider-session-1".to_owned(),
                transcript_path: Some(r"C:\private\provider-transcript.jsonl".to_owned()),
            }),
            active_session: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 11,
            last_error: None,
        };
        (node_id, workspaces, record)
    }

    #[test]
    fn durable_state_round_trips_without_ephemeral_active_binding() {
        let path = temp_path("round-trip");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.provider_session.as_mut().unwrap().transcript_path =
            Some("C:\\private\\bearer-secret\nprovider-transcript.jsonl".to_owned());
        save(Some(&path), &node_id, &workspaces, &[record.clone()]).unwrap();
        let persisted = fs::read_to_string(&path).unwrap();
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
            provider_session_semantic_key(AgentProvider::Claude, &without_path),
            provider_session_semantic_key(AgentProvider::Claude, &with_path),
        );
        assert_ne!(
            provider_session_semantic_key(AgentProvider::Claude, &without_path),
            provider_session_semantic_key(AgentProvider::Codex, &with_path),
        );
        let conversation = ProviderSessionIdentity {
            key: ProviderSessionKey::ConversationId,
            ..with_path
        };
        assert_ne!(
            provider_session_semantic_key(AgentProvider::Claude, &without_path),
            provider_session_semantic_key(AgentProvider::Claude, &conversation),
        );
    }

    #[test]
    fn legacy_state_transcript_path_is_removed_from_loaded_identity() {
        let path = temp_path("legacy-transcript-path");
        let (node_id, workspaces, mut record) = fixture(&path);
        record.provider_session.as_mut().unwrap().transcript_path =
            Some("C:\\private\\bearer-secret\nprovider-transcript.jsonl".to_owned());
        let legacy = PersistedNodeState {
            version: STATE_VERSION,
            node_id: node_id.clone(),
            workspaces: workspaces
                .iter()
                .map(|(workspace_id, canonical_root)| PersistedWorkspace {
                    workspace_id: workspace_id.clone(),
                    canonical_root: canonical_root.clone(),
                })
                .collect(),
            session_records: vec![record],
        };
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
        let legacy = PersistedNodeState {
            version: STATE_VERSION,
            node_id: node_id.clone(),
            workspaces: workspaces
                .iter()
                .map(|(workspace_id, canonical_root)| PersistedWorkspace {
                    workspace_id: workspace_id.clone(),
                    canonical_root: canonical_root.clone(),
                })
                .collect(),
            session_records: vec![record, duplicate],
        };
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
