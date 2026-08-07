//! Stable, serde-only observer API contract for Gate4Agent C2.

pub use gate4agent_node_protocol::{NodeCursor, NodeId};
use gate4agent_node_protocol::{AgentProvider, NodeSnapshot, WorkspaceId};
use gate4agent_types::{
    AgentInstanceId, SessionGeneration, SessionStatus, TerminalSize, TransportKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const C2_API_VERSION: u16 = 1;
pub const DEFAULT_C2_API_LISTEN: &str = "127.0.0.1:18320";
pub const MAX_C2_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_C2_NODES: usize = 64;
pub const MAX_C2_ENDPOINT_BYTES: usize = 1024;
pub const MAX_C2_WORKSPACES_PER_NODE: usize = 32;
pub const MAX_C2_SESSIONS_PER_NODE: usize = 128;
pub const MAX_C2_GAPS_PER_NODE: usize = 64;
pub const MAX_C2_ROOT_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeTransportState {
    Online,
    Offline,
    Parked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeFreshness {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    IncarnationChanged,
    HistoryEvicted,
    NonContiguousEvents,
    CursorRegression,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeGap {
    pub kind: GapKind,
    pub detected_at_unix_ms: u64,
    pub previous: Option<NodeCursor>,
    pub observed: NodeCursor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum C2ErrorCategory {
    Authentication,
    Identity,
    Protocol,
    Transport,
    Timeout,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SanitizedError {
    pub category: C2ErrorCategory,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimSession {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub agent_id: String,
    pub transport: TransportKind,
    pub status: SlimSessionStatus,
    pub process_id: Option<u32>,
    pub terminal_size: Option<TerminalSize>,
    pub operation_pending: bool,
    pub input_pending: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlimSessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

impl From<&SessionStatus> for SlimSessionStatus {
    fn from(status: &SessionStatus) -> Self {
        match status {
            SessionStatus::Registered => Self::Registered,
            SessionStatus::Starting => Self::Starting,
            SessionStatus::Running => Self::Running,
            SessionStatus::Stopping => Self::Stopping,
            SessionStatus::Exited { .. } => Self::Exited,
            SessionStatus::Failed { .. } => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimWorkspace {
    pub workspace_id: WorkspaceId,
    pub canonical_root: String,
    pub canonical_root_truncated: bool,
    pub sessions: Vec<SlimSession>,
    pub session_count: usize,
    pub sessions_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimNodeInventory {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentProvider>,
    pub workspaces: BTreeMap<WorkspaceId, SlimWorkspace>,
    pub workspace_count: usize,
    pub workspaces_truncated: bool,
    pub session_count: usize,
    pub sessions_truncated: bool,
}

impl SlimNodeInventory {
    pub fn from_snapshot(snapshot: &NodeSnapshot) -> Self {
        let mut providers = snapshot.enabled_providers.clone();
        providers.sort_by_key(|provider| provider.agent_id());
        providers.dedup();
        let workspace_count = snapshot.workspaces.len();
        let session_count = snapshot.workspaces.iter().map(|workspace| workspace.sessions.len()).sum();
        let mut remaining_sessions = MAX_C2_SESSIONS_PER_NODE;
        let mut workspaces = BTreeMap::new();
        let mut ordered = snapshot.workspaces.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
        for workspace in ordered.into_iter().take(MAX_C2_WORKSPACES_PER_NODE) {
            let mut sessions = workspace.sessions.iter().collect::<Vec<_>>();
            sessions.sort_by_key(|session| (session.instance_id, session.generation));
            let take = remaining_sessions.min(sessions.len());
            let slim_sessions = sessions.into_iter().take(take).map(|session| SlimSession {
                instance_id: session.instance_id,
                generation: session.generation,
                agent_id: session.agent_id.as_str().to_owned(),
                transport: session.transport,
                status: SlimSessionStatus::from(&session.status),
                process_id: session.process_id,
                terminal_size: session.terminal_size,
                operation_pending: session.pending_operation.is_some(),
                input_pending: session.pending_input.is_some(),
            }).collect();
            remaining_sessions -= take;
            let (canonical_root, canonical_root_truncated) = truncate_utf8(&workspace.canonical_root, MAX_C2_ROOT_BYTES);
            workspaces.insert(workspace.workspace_id.clone(), SlimWorkspace {
                workspace_id: workspace.workspace_id.clone(),
                canonical_root,
                canonical_root_truncated,
                sessions: slim_sessions,
                session_count: workspace.sessions.len(),
                sessions_truncated: workspace.sessions.len() > take,
            });
        }
        let included_session_count = workspaces
            .values()
            .map(|workspace| workspace.sessions.len())
            .sum::<usize>();
        Self {
            node_id: snapshot.node_id.clone(),
            enabled_providers: providers,
            workspaces,
            workspace_count,
            workspaces_truncated: workspace_count > MAX_C2_WORKSPACES_PER_NODE,
            session_count,
            sessions_truncated: included_session_count < session_count,
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedNode {
    pub endpoint: String,
    pub transport_label: String,
    pub transport: NodeTransportState,
    pub freshness: NodeFreshness,
    pub cursor: Option<NodeCursor>,
    pub inventory: Option<SlimNodeInventory>,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub last_error: Option<SanitizedError>,
    pub gaps: Vec<NodeGap>,
    pub gaps_truncated: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
    pub api_version: u16,
    pub pid: u32,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadyResponse {
    pub ready: bool,
    pub api_version: u16,
    pub configured_nodes: usize,
    pub attempted_nodes: usize,
    pub online_nodes: usize,
    pub offline_nodes: usize,
    pub parked_nodes: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusResponse {
    pub api_version: u16,
    pub ready: bool,
    pub observed_at_unix_ms: u64,
    pub nodes: BTreeMap<NodeId, ObservedNode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{NodeSnapshot, WorkspaceSnapshot};
    use gate4agent_types::{
        AgentId, AgentInstanceId, CapabilitySnapshot, ForegroundSnapshot, HistorySnapshot,
        OperationId, PreparedInputKind, ProviderSnapshot, ResumeSnapshot, SessionGeneration,
        SessionSnapshot, SessionStatus, TerminalSize, TransportKind,
    };

    fn fixture_session() -> SessionSnapshot {
        SessionSnapshot {
            instance_id: AgentInstanceId(7),
            agent_id: AgentId::new("codex").unwrap(),
            transport: TransportKind::Pty,
            generation: SessionGeneration(2),
            status: SessionStatus::Running,
            pending_operation: Some(OperationId(9)),
            pending_input: Some(PreparedInputKind::TerminalText),
            process_id: Some(1234),
            terminal_size: Some(TerminalSize { rows: 40, columns: 120 }),
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: CapabilitySnapshot::default(),
            history: HistorySnapshot::default(),
            resume: ResumeSnapshot::default(),
            foreground: ForegroundSnapshot::default(),
            provider: ProviderSnapshot::default(),
        }
    }

    #[test]
    fn slim_inventory_is_deterministic_and_excludes_terminal_history() {
        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentProvider::Codex, AgentProvider::Claude, AgentProvider::Codex],
            workspaces: vec![
                WorkspaceSnapshot { workspace_id: WorkspaceId::new("z-work").unwrap(), canonical_root: "z".to_owned(), sessions: Vec::new() },
                WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("a-work").unwrap(),
                    canonical_root: "a".to_owned(),
                    sessions: vec![fixture_session()],
                },
            ],
        };
        let slim = SlimNodeInventory::from_snapshot(&snapshot);
        assert_eq!(slim.enabled_providers, vec![AgentProvider::Claude, AgentProvider::Codex]);
        assert_eq!(slim.workspaces.keys().map(WorkspaceId::as_str).collect::<Vec<_>>(), vec!["a-work", "z-work"]);
        let session = &slim.workspaces[&WorkspaceId::new("a-work").unwrap()].sessions[0];
        assert_eq!(session.transport, TransportKind::Pty);
        assert_eq!(session.process_id, Some(1234));
        assert_eq!(session.terminal_size, Some(TerminalSize { rows: 40, columns: 120 }));
        assert!(session.operation_pending);
        assert!(session.input_pending);
        let json = serde_json::to_string(&slim).unwrap();
        assert!(!json.contains("terminal_frame"));
        assert!(!json.contains("history"));
    }

    #[test]
    fn slim_inventory_reports_sessions_hidden_by_workspace_truncation() {
        let mut workspaces = (0..MAX_C2_WORKSPACES_PER_NODE)
            .map(|index| WorkspaceSnapshot {
                workspace_id: WorkspaceId::new(format!("work-{index:02}")).unwrap(),
                canonical_root: format!("root-{index:02}"),
                sessions: Vec::new(),
            })
            .collect::<Vec<_>>();
        workspaces.push(WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("work-zz").unwrap(),
            canonical_root: "hidden-root".to_owned(),
            sessions: vec![fixture_session()],
        });
        let slim = SlimNodeInventory::from_snapshot(&NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            workspaces,
        });
        assert!(slim.workspaces_truncated);
        assert_eq!(slim.session_count, 1);
        assert!(slim.sessions_truncated);
    }
}
