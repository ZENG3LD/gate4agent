//! Managed worktree lease records (persisted shape shared with the session registry).

use crate::protocol::{
    ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseId, ManagedWorktreeLeaseSnapshot,
    ManagedWorktreeLeaseState, ManagedWorktreeRetention, NodeIncarnationId, SessionRecordId,
    WorktreeProfileId, WorktreeProfileRevision, WorkspaceId,
};
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedWorktreeSessionHolder {
    pub incarnation_id: NodeIncarnationId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedWorktreeLeaseRecord {
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
    pub fn snapshot(&self) -> ManagedWorktreeLeaseSnapshot {
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

    pub fn has_holders(&self) -> bool {
        !self.session_holders.is_empty() || !self.record_holders.is_empty()
    }
}

