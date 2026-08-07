//! Bounded wire contract for the local Gate4Agent node.

use gate4agent_types::{
    AgentInstanceId, ControlEvent, SessionGeneration, SessionSnapshot, TerminalControl,
    TerminalSize,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Borrow;
use std::fmt;
use std::io;
use std::str::FromStr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Duration};

pub const NODE_PROTOCOL_VERSION: u16 = 6;
pub const MAX_NODE_IDENTIFIER_BYTES: usize = 64;
pub const MAX_NODE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODE_CLIENT_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_NODE_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_NODE_TERMINAL_BYTES: usize = 64;
pub const MAX_WORKSPACE_ROOT_BYTES: usize = gate4agent_types::WORKING_DIRECTORY_MAX_BYTES;
pub const MAX_NODE_HELLO_FRAME_BYTES: usize = 8 * 1024;
pub const NODE_AUTH_NONCE_BYTES: usize = 32;
pub const NODE_AUTH_PROOF_BYTES: usize = 32;
pub const MAX_CONTROLLER_LEASE_MS: u64 = 60_000;
pub const MIN_CONTROLLER_LEASE_MS: u64 = 1_000;
pub const DEFAULT_CONTROLLER_LEASE_MS: u64 = 15_000;
pub const DEFAULT_NODE_ENDPOINT: &str = r"\\.\pipe\gate4agent-node";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientRole {
    Operator,
    Observer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    Claude,
    Codex,
    Kimi,
}

impl AgentProvider {
    pub fn agent_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    Pty,
    Inline,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceId(String);

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub canonical_root: String,
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
    pub relative_path: String,
    pub kind: WorkspaceEntryKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitStatusEntry {
    pub index_status: String,
    pub worktree_status: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitCommitSummary {
    pub id: String,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitWorktreeSnapshot {
    pub path: String,
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
pub struct NodeSnapshot {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentProvider>,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    pub protocol_version: u16,
    pub role: ClientRole,
    pub client_nonce: [u8; NODE_AUTH_NONCE_BYTES],
}

impl ClientHello {
    pub fn new(role: ClientRole, client_nonce: [u8; NODE_AUTH_NONCE_BYTES]) -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            role,
            client_nonce,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerChallenge {
    pub protocol_version: u16,
    pub server_nonce: [u8; NODE_AUTH_NONCE_BYTES],
    pub server_proof: [u8; NODE_AUTH_PROOF_BYTES],
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeHello {
    pub protocol_version: u16,
    pub connection_id: u64,
    pub role: ClientRole,
    pub event_sequence: u64,
    pub controller: Option<ControllerState>,
    pub snapshot: NodeSnapshot,
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
    AcquireController { lease_ms: u64 },
    ReleaseController,
    RegisterWorkspace {
        workspace_id: WorkspaceId,
        root: String,
    },
    UnregisterWorkspace {
        workspace_id: WorkspaceId,
    },
    CreateWorktree {
        source_workspace_id: WorkspaceId,
        workspace_id: WorkspaceId,
        target_root: String,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree {
        source_workspace_id: WorkspaceId,
        target_root: String,
    },
    Spawn {
        workspace_id: WorkspaceId,
        provider: AgentProvider,
        mode: SessionMode,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    Resume {
        session: SessionAddress,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
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
    Controller {
        controller: Option<ControllerState>,
    },
    SpawnAccepted {
        session: SessionAddress,
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
        target_root: String,
        workspace_id: Option<WorkspaceId>,
    },
    Accepted,
    ShuttingDown,
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
    Unauthorized,
    ObserverReadOnly,
    ControllerBusy,
    ControllerRequired,
    UnknownWorkspace,
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
    UnknownSession,
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
    ControllerChanged { controller: Option<ControllerState> },
    WorkspaceAdded { workspace: WorkspaceSnapshot },
    WorkspaceRemoved { workspace_id: WorkspaceId },
    ResyncRequired { oldest_available_sequence: u64 },
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
    fn protocol_v6_workspace_and_worktree_mutations_have_exact_bounded_wire_shapes() {
        assert_eq!(NODE_PROTOCOL_VERSION, 6);
        assert_eq!(MAX_WORKSPACE_ROOT_BYTES, gate4agent_types::WORKING_DIRECTORY_MAX_BYTES);

        let register = NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            root: r"C:\repo-2".to_owned(),
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
            target_root: r"C:\trees\topic-one".to_owned(),
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
            target_root: r"C:\trees\topic-one".to_owned(),
        };
        let remove_json = serde_json::to_string(&remove).unwrap();
        assert_eq!(
            remove_json,
            r#"{"kind":"remove-worktree","source_workspace_id":"primary","target_root":"C:\\trees\\topic-one"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&remove_json).unwrap(), remove);
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
    fn workspace_responses_and_events_round_trip_without_client_only_state() {
        let workspace = WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            canonical_root: r"C:\repo-2".to_owned(),
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
                path: r"C:\trees\topic-one".to_owned(),
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
            target_root: r"C:\trees\topic-one".to_owned(),
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
                        relative_path: "src".to_owned(),
                        kind: WorkspaceEntryKind::Directory,
                    },
                    WorkspaceEntry {
                        relative_path: "src/lib.rs".to_owned(),
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
                        path: "src/lib.rs".to_owned(),
                    }],
                    recent_commits: vec![GitCommitSummary {
                        id: "abc1234".to_owned(),
                        summary: "bounded summary".to_owned(),
                    }],
                    worktrees: vec![GitWorktreeSnapshot {
                        path: r"C:\repo".to_owned(),
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
}
