use crate::git_worktree::{paths_equal, validate_managed_allocation_root};
use crate::protocol::{
    ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseId, ManagedWorktreeLeaseSnapshot,
    ManagedWorktreeLeaseState, ManagedWorktreeRetention, NodeIncarnationId, SessionRecordId,
    WorktreeProfileId, WorktreeProfileRevision, WorkspaceId, MAX_MANAGED_WORKTREE_LEASES,
};
use gate4agent_node_wire::random_nonce;
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MAX_MANAGED_WORKTREE_TOMBSTONES: usize = MAX_MANAGED_WORKTREE_LEASES;
const MAX_BRANCH_PREFIX_BYTES: usize = 256;
const MAX_BASE_REVISION_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorktreeProfile {
    profile_id: WorktreeProfileId,
    revision: WorktreeProfileRevision,
    allocation_root: String,
    branch_prefix: String,
    base: String,
    retention: ManagedWorktreeRetention,
}

impl ManagedWorktreeProfile {
    pub fn new(
        profile_id: WorktreeProfileId,
        revision: WorktreeProfileRevision,
        allocation_root: impl AsRef<Path>,
        branch_prefix: impl Into<String>,
        base: impl Into<String>,
        retention: ManagedWorktreeRetention,
    ) -> Result<Self, String> {
        let allocation_root = allocation_root.as_ref();
        if !allocation_root.is_absolute() {
            return Err("managed worktree allocation root must be absolute".to_owned());
        }
        let canonical = std::fs::canonicalize(allocation_root)
            .map_err(|error| format!("managed worktree allocation root is unavailable: {error}"))?;
        if !canonical.is_dir() {
            return Err("managed worktree allocation root must be a directory".to_owned());
        }
        let allocation_root = canonical.into_os_string().into_string().map_err(|path| {
            format!(
                "managed worktree allocation root is not valid Unicode: {}",
                path.to_string_lossy(),
            )
        })?;
        let allocation_root = crate::platform::normalize_canonical_root(allocation_root);
        let branch_prefix = branch_prefix.into();
        if branch_prefix.is_empty()
            || branch_prefix.len() > MAX_BRANCH_PREFIX_BYTES
            || branch_prefix.chars().any(char::is_control)
            || branch_prefix.ends_with('/')
        {
            return Err("managed worktree branch prefix is invalid".to_owned());
        }
        let base = base.into();
        if base.is_empty()
            || base.len() > MAX_BASE_REVISION_BYTES
            || base.chars().any(char::is_control)
        {
            return Err("managed worktree base revision is invalid".to_owned());
        }
        Ok(Self {
            profile_id,
            revision,
            allocation_root,
            branch_prefix,
            base,
            retention,
        })
    }

    pub fn profile_id(&self) -> &WorktreeProfileId {
        &self.profile_id
    }

    pub fn revision(&self) -> &WorktreeProfileRevision {
        &self.revision
    }

    pub fn allocation_root(&self) -> &str {
        &self.allocation_root
    }

    pub fn branch_prefix(&self) -> &str {
        &self.branch_prefix
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn retention(&self) -> ManagedWorktreeRetention {
        self.retention
    }

    pub(crate) fn validate_for_workspace(&self, source_root: &str) -> Result<(), String> {
        validate_managed_allocation_root(source_root, &self.allocation_root)
            .map_err(|error| error.message)
    }

    pub(crate) fn validate_target_authority(
        &self,
        lease_id: &ManagedWorktreeLeaseId,
        target_root: &str,
    ) -> Result<(), String> {
        let current_root = std::fs::canonicalize(&self.allocation_root)
            .map_err(|error| format!("managed worktree allocation root is unavailable: {error}"))?;
        let current_root = crate::platform::normalize_canonical_root(
            current_root.into_os_string().into_string()
                .map_err(|_| "managed worktree allocation root is not valid Unicode".to_owned())?,
        );
        if !paths_equal(&current_root, &self.allocation_root) {
            return Err("managed worktree allocation root identity changed".to_owned());
        }
        let target = Path::new(target_root);
        let expected = PathBuf::from(&self.allocation_root).join(lease_id.as_str());
        if target != expected
            || target.file_name().and_then(|name| name.to_str()) != Some(lease_id.as_str())
            || target.components().any(|component| matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir,
            ))
        {
            return Err("managed worktree target is not the exact allocated lease child".to_owned());
        }
        let parent = target.parent()
            .ok_or_else(|| "managed worktree target has no parent".to_owned())?;
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|error| format!("managed worktree target parent is unavailable: {error}"))?;
        let canonical_parent = crate::platform::normalize_canonical_root(
            canonical_parent.into_os_string().into_string()
                .map_err(|_| "managed worktree target parent is not valid Unicode".to_owned())?,
        );
        if !paths_equal(&canonical_parent, &self.allocation_root)
        {
            return Err("managed worktree target is not an exact child of its allocation root".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ManagedWorktreeSessionHolder {
    pub incarnation_id: NodeIncarnationId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ManagedWorktreeLeaseRecord {
    pub lease_id: ManagedWorktreeLeaseId,
    pub source_workspace_id: WorkspaceId,
    pub workspace_id: WorkspaceId,
    pub profile_id: WorktreeProfileId,
    pub profile_revision: WorktreeProfileRevision,
    pub target_root: String,
    pub branch: String,
    pub base_commit: String,
    pub expected_head: Option<String>,
    pub retention: ManagedWorktreeRetention,
    pub state: ManagedWorktreeLeaseState,
    pub session_holders: Vec<ManagedWorktreeSessionHolder>,
    pub record_holders: Vec<SessionRecordId>,
    pub cleanup_failure: Option<ManagedWorktreeCleanupFailure>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl ManagedWorktreeLeaseRecord {
    pub(crate) fn snapshot(&self) -> ManagedWorktreeLeaseSnapshot {
        ManagedWorktreeLeaseSnapshot {
            lease_id: self.lease_id.clone(),
            source_workspace_id: self.source_workspace_id.clone(),
            workspace_id: self.workspace_id.clone(),
            profile_id: self.profile_id.clone(),
            profile_revision: self.profile_revision.clone(),
            retention: self.retention,
            state: self.state,
            active_session_count: u16::try_from(self.session_holders.len()).unwrap_or(u16::MAX),
            managed_record_count: u16::try_from(self.record_holders.len()).unwrap_or(u16::MAX),
            cleanup_failure: self.cleanup_failure,
            created_at_unix_ms: self.created_at_unix_ms,
            updated_at_unix_ms: self.updated_at_unix_ms,
        }
    }

    pub(crate) fn has_holders(&self) -> bool {
        !self.session_holders.is_empty() || !self.record_holders.is_empty()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedWorktreeRegistry {
    leases: BTreeMap<ManagedWorktreeLeaseId, ManagedWorktreeLeaseRecord>,
    tombstones: BTreeMap<ManagedWorktreeLeaseId, ManagedWorktreeLeaseRecord>,
}

impl ManagedWorktreeRegistry {
    pub(crate) fn from_records(
        records: Vec<ManagedWorktreeLeaseRecord>,
        tombstones: Vec<ManagedWorktreeLeaseRecord>,
    ) -> Result<Self, String> {
        if records.len() > MAX_MANAGED_WORKTREE_LEASES
            || tombstones.len() > MAX_MANAGED_WORKTREE_TOMBSTONES
        {
            return Err("managed worktree durable registry exceeds its bounded capacity".to_owned());
        }
        let mut registry = Self::default();
        let mut workspace_ids = BTreeSet::new();
        let mut target_roots = BTreeSet::new();
        let mut branches = BTreeSet::new();
        for record in records {
            if record.state == ManagedWorktreeLeaseState::Removed
                || !workspace_ids.insert(record.workspace_id.clone())
                || !target_roots.insert(crate::platform::root_identity(&record.target_root))
                || !branches.insert(record.branch.clone())
                || registry.leases.insert(record.lease_id.clone(), record).is_some()
            {
                return Err("managed worktree durable registry is inconsistent".to_owned());
            }
        }
        for record in tombstones {
            if record.state != ManagedWorktreeLeaseState::Removed
                || registry.leases.contains_key(&record.lease_id)
                || registry.tombstones.insert(record.lease_id.clone(), record).is_some()
            {
                return Err("managed worktree tombstone registry is inconsistent".to_owned());
            }
        }
        Ok(registry)
    }

    pub(crate) fn records(&self) -> Vec<ManagedWorktreeLeaseRecord> {
        self.leases.values().cloned().collect()
    }

    pub(crate) fn tombstones(&self) -> Vec<ManagedWorktreeLeaseRecord> {
        self.tombstones.values().cloned().collect()
    }

    pub(crate) fn snapshots(&self) -> Vec<ManagedWorktreeLeaseSnapshot> {
        self.leases.values().map(ManagedWorktreeLeaseRecord::snapshot).collect()
    }

    pub(crate) fn get(&self, lease_id: &ManagedWorktreeLeaseId) -> Option<&ManagedWorktreeLeaseRecord> {
        self.leases.get(lease_id).or_else(|| self.tombstones.get(lease_id))
    }

    pub(crate) fn get_mut(
        &mut self,
        lease_id: &ManagedWorktreeLeaseId,
    ) -> Option<&mut ManagedWorktreeLeaseRecord> {
        self.leases.get_mut(lease_id)
    }

    pub(crate) fn active_records(&self) -> impl Iterator<Item = &ManagedWorktreeLeaseRecord> {
        self.leases.values()
    }

    pub(crate) fn allocate(
        &mut self,
        source_workspace_id: WorkspaceId,
        profile: &ManagedWorktreeProfile,
        base_commit: String,
        now: u64,
    ) -> Result<ManagedWorktreeLeaseRecord, String> {
        if self.leases.len() >= MAX_MANAGED_WORKTREE_LEASES {
            return Err("managed worktree lease capacity is full".to_owned());
        }
        for _ in 0..8 {
            let nonce = random_nonce()
                .map_err(|error| format!("managed worktree identity generation failed: {error}"))?;
            let suffix = hex_suffix(&nonce[..12]);
            let lease_id = ManagedWorktreeLeaseId::new(format!("mw-{suffix}"))
                .map_err(|error| error.to_string())?;
            if self.leases.contains_key(&lease_id) || self.tombstones.contains_key(&lease_id) {
                continue;
            }
            let workspace_id = WorkspaceId::new(format!("managed-{suffix}"))
                .map_err(|error| error.to_string())?;
            let target = PathBuf::from(profile.allocation_root()).join(lease_id.as_str());
            let target_root = target.into_os_string().into_string().map_err(|path| {
                format!("managed worktree target is not valid Unicode: {}", path.to_string_lossy())
            })?;
            let branch = format!("{}/{}", profile.branch_prefix(), lease_id.as_str());
            let record = ManagedWorktreeLeaseRecord {
                lease_id: lease_id.clone(),
                source_workspace_id,
                workspace_id,
                profile_id: profile.profile_id().clone(),
                profile_revision: profile.revision().clone(),
                target_root,
                branch,
                base_commit,
                expected_head: None,
                retention: profile.retention(),
                state: ManagedWorktreeLeaseState::Allocating,
                session_holders: Vec::new(),
                record_holders: Vec::new(),
                cleanup_failure: None,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
            };
            self.leases.insert(lease_id, record.clone());
            return Ok(record);
        }
        Err("could not allocate a unique managed worktree identity".to_owned())
    }

    pub(crate) fn remove_unmutated(&mut self, lease_id: &ManagedWorktreeLeaseId) {
        self.leases.remove(lease_id);
    }

    pub(crate) fn tombstone(
        &mut self,
        lease_id: &ManagedWorktreeLeaseId,
        now: u64,
    ) -> Option<ManagedWorktreeLeaseRecord> {
        let mut record = self.leases.remove(lease_id)?;
        record.state = ManagedWorktreeLeaseState::Removed;
        record.session_holders.clear();
        record.record_holders.clear();
        record.cleanup_failure = None;
        record.updated_at_unix_ms = now;
        while self.tombstones.len() >= MAX_MANAGED_WORKTREE_TOMBSTONES {
            let oldest = self
                .tombstones
                .values()
                .min_by_key(|item| (item.updated_at_unix_ms, item.lease_id.as_str()))
                .map(|item| item.lease_id.clone());
            if let Some(oldest) = oldest {
                self.tombstones.remove(&oldest);
            } else {
                break;
            }
        }
        self.tombstones.insert(record.lease_id.clone(), record.clone());
        Some(record)
    }

    pub(crate) fn clear_stale_session_holders(&mut self, incarnation: NodeIncarnationId, now: u64) {
        for lease in self.leases.values_mut() {
            let previous = lease.session_holders.len();
            lease.session_holders.retain(|holder| holder.incarnation_id == incarnation);
            if previous != lease.session_holders.len() {
                lease.updated_at_unix_ms = now;
            }
        }
    }

    pub(crate) fn bind_session(
        &mut self,
        lease_id: &ManagedWorktreeLeaseId,
        holder: ManagedWorktreeSessionHolder,
        record_id: Option<SessionRecordId>,
        now: u64,
    ) -> Result<ManagedWorktreeLeaseSnapshot, String> {
        let lease = self.leases.get_mut(lease_id)
            .ok_or_else(|| "managed worktree lease does not exist".to_owned())?;
        if !lease.session_holders.contains(&holder) {
            lease.session_holders.push(holder);
        }
        if let Some(record_id) = record_id {
            if !lease.record_holders.contains(&record_id) {
                lease.record_holders.push(record_id);
            }
        }
        lease.state = ManagedWorktreeLeaseState::InUse;
        lease.cleanup_failure = None;
        lease.updated_at_unix_ms = now;
        Ok(lease.snapshot())
    }

    pub(crate) fn release_session(
        &mut self,
        lease_id: &ManagedWorktreeLeaseId,
        incarnation_id: NodeIncarnationId,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        now: u64,
    ) -> Option<ManagedWorktreeLeaseSnapshot> {
        let lease = self.leases.get_mut(lease_id)?;
        lease.session_holders.retain(|holder| {
            holder.incarnation_id != incarnation_id
                || holder.instance_id != instance_id
                || holder.generation != generation
        });
        if !lease.has_holders() && lease.state == ManagedWorktreeLeaseState::InUse {
            lease.state = if lease.retention == ManagedWorktreeRetention::Retain {
                ManagedWorktreeLeaseState::Retained
            } else {
                ManagedWorktreeLeaseState::Ready
            };
        }
        lease.updated_at_unix_ms = now;
        Some(lease.snapshot())
    }

    pub(crate) fn lease_for_workspace(&self, workspace_id: &WorkspaceId) -> Option<ManagedWorktreeLeaseId> {
        self.leases.values().find(|lease| &lease.workspace_id == workspace_id)
            .map(|lease| lease.lease_id.clone())
    }

    pub(crate) fn reattach_record_holders(&mut self, record_ids: &[(SessionRecordId, WorkspaceId)], now: u64) {
        for lease in self.leases.values_mut() {
            let next_holders = record_ids.iter()
                .filter(|(_, workspace_id)| workspace_id == &lease.workspace_id)
                .map(|(record_id, _)| record_id.clone())
                .collect::<Vec<_>>();
            let previous_holders = lease.record_holders.clone();
            let previous_state = lease.state;
            lease.record_holders = next_holders;
            if !matches!(
                lease.state,
                ManagedWorktreeLeaseState::RecoveryRequired
                    | ManagedWorktreeLeaseState::CleanupBlocked
                    | ManagedWorktreeLeaseState::Removed
                    | ManagedWorktreeLeaseState::Allocating
            ) {
                if lease.has_holders() {
                    lease.state = ManagedWorktreeLeaseState::InUse;
                } else if previous_state == ManagedWorktreeLeaseState::InUse {
                    lease.state = if lease.retention == ManagedWorktreeRetention::Retain {
                        ManagedWorktreeLeaseState::Retained
                    } else {
                        ManagedWorktreeLeaseState::Ready
                    };
                }
            }
            if lease.state != previous_state || lease.record_holders != previous_holders {
                lease.updated_at_unix_ms = now;
            }
        }
    }
}

pub(crate) fn exact_owned_worktree(
    lease: &ManagedWorktreeLeaseRecord,
    path: &str,
    branch: Option<&str>,
    _head: &str,
) -> bool {
    paths_equal(&lease.target_root, path)
        && branch == Some(lease.branch.as_str())
}

pub(crate) fn exact_created_worktree(
    lease: &ManagedWorktreeLeaseRecord,
    path: &str,
    branch: Option<&str>,
    head: &str,
) -> bool {
    exact_owned_worktree(lease, path, branch, head)
        && lease.expected_head.as_deref().unwrap_or(lease.base_commit.as_str()) == head
}

fn hex_suffix(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut value, "{byte:02x}");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temporary_root(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gate4agent-managed-worktree-{label}-{}-{sequence}",
            std::process::id(),
        ))
    }

    fn lease(
        id: &str,
        workspace: &str,
        target: &str,
        branch: &str,
    ) -> ManagedWorktreeLeaseRecord {
        ManagedWorktreeLeaseRecord {
            lease_id: ManagedWorktreeLeaseId::new(id).unwrap(),
            source_workspace_id: WorkspaceId::new("source").unwrap(),
            workspace_id: WorkspaceId::new(workspace).unwrap(),
            profile_id: WorktreeProfileId::new("default").unwrap(),
            profile_revision: WorktreeProfileRevision::new("v1").unwrap(),
            target_root: target.to_owned(),
            branch: branch.to_owned(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            expected_head: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            retention: ManagedWorktreeRetention::RemoveWhenReleased,
            state: ManagedWorktreeLeaseState::Ready,
            session_holders: Vec::new(),
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    #[test]
    fn managed_profile_recanonicalizes_root_and_requires_exact_lease_child() {
        let parent = temporary_root("profile");
        let source = parent.join("source");
        let allocation = parent.join("allocation");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&allocation).unwrap();
        let profile = ManagedWorktreeProfile::new(
            WorktreeProfileId::new("default").unwrap(),
            WorktreeProfileRevision::new("v1").unwrap(),
            &allocation,
            "gate4agent",
            "HEAD",
            ManagedWorktreeRetention::RemoveWhenReleased,
        ).unwrap();
        profile.validate_for_workspace(source.to_str().unwrap()).unwrap();
        let lease_id = ManagedWorktreeLeaseId::new("mw-abc").unwrap();
        let exact = PathBuf::from(profile.allocation_root()).join(lease_id.as_str());
        profile.validate_target_authority(&lease_id, exact.to_str().unwrap()).unwrap();
        let traversing = PathBuf::from(profile.allocation_root())
            .join("ignored").join("..").join(lease_id.as_str());
        assert!(profile.validate_target_authority(&lease_id, traversing.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn registry_rejects_duplicate_workspace_root_and_branch_authority() {
        let first = lease("mw-a", "managed-a", "C:/trees/a", "gate4agent/a");
        let mut duplicate_workspace = lease("mw-b", "managed-a", "C:/trees/b", "gate4agent/b");
        assert!(ManagedWorktreeRegistry::from_records(
            vec![first.clone(), duplicate_workspace.clone()], Vec::new(),
        ).is_err());
        duplicate_workspace.workspace_id = WorkspaceId::new("managed-b").unwrap();
        duplicate_workspace.target_root = first.target_root.clone();
        assert!(ManagedWorktreeRegistry::from_records(
            vec![first.clone(), duplicate_workspace.clone()], Vec::new(),
        ).is_err());
        duplicate_workspace.target_root = "C:/trees/b".to_owned();
        duplicate_workspace.branch = first.branch.clone();
        assert!(ManagedWorktreeRegistry::from_records(
            vec![first, duplicate_workspace], Vec::new(),
        ).is_err());
    }

    #[test]
    fn exact_current_incarnation_holder_release_transitions_without_head_fence() {
        let mut registry = ManagedWorktreeRegistry::from_records(
            vec![lease("mw-a", "managed-a", "C:/trees/a", "gate4agent/a")],
            Vec::new(),
        ).unwrap();
        let lease_id = ManagedWorktreeLeaseId::new("mw-a").unwrap();
        let current = NodeIncarnationId::from_bytes([1; crate::protocol::NODE_INCARNATION_ID_BYTES]);
        registry.bind_session(
            &lease_id,
            ManagedWorktreeSessionHolder {
                incarnation_id: current,
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
            None,
            2,
        ).unwrap();
        registry.release_session(
            &lease_id,
            NodeIncarnationId::from_bytes([2; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            AgentInstanceId(7),
            SessionGeneration(2),
            3,
        );
        assert_eq!(registry.get(&lease_id).unwrap().snapshot().active_session_count, 1);
        registry.release_session(&lease_id, current, AgentInstanceId(7), SessionGeneration(2), 4);
        assert_eq!(registry.get(&lease_id).unwrap().state, ManagedWorktreeLeaseState::Ready);
        let record_id = SessionRecordId::new("sr-a").unwrap();
        registry.reattach_record_holders(
            &[(record_id, WorkspaceId::new("managed-a").unwrap())],
            5,
        );
        assert_eq!(registry.get(&lease_id).unwrap().state, ManagedWorktreeLeaseState::InUse);
        assert_eq!(registry.get(&lease_id).unwrap().updated_at_unix_ms, 5);
        registry.reattach_record_holders(
            &[(SessionRecordId::new("sr-a").unwrap(), WorkspaceId::new("managed-a").unwrap())],
            9,
        );
        assert_eq!(registry.get(&lease_id).unwrap().updated_at_unix_ms, 5);
        let record = registry.get(&lease_id).unwrap();
        assert!(exact_owned_worktree(record, "C:/trees/a", Some("gate4agent/a"), "new-head"));
        assert!(!exact_created_worktree(record, "C:/trees/a", Some("gate4agent/a"), "new-head"));
    }
}
