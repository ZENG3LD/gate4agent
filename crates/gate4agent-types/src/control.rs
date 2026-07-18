use crate::{AgentId, InputAction, InputPrepareError, PreparedInput, PreparedInputKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentInstanceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub u64);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionGeneration(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    Pty,
    Pipe,
    Acp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub protocol_version: u16,
    pub id: CommandId,
    pub command: ControlCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlCommand {
    Register {
        instance_id: AgentInstanceId,
        agent_id: AgentId,
        transport: TransportKind,
    },
    Start {
        instance_id: AgentInstanceId,
    },
    Stop {
        instance_id: AgentInstanceId,
        force: bool,
    },
    SendInput {
        instance_id: AgentInstanceId,
        action: InputAction,
    },
    Remove {
        instance_id: AgentInstanceId,
    },
}

impl ControlCommand {
    pub fn instance_id(&self) -> AgentInstanceId {
        match self {
            Self::Register { instance_id, .. }
            | Self::Start { instance_id }
            | Self::Stop { instance_id, .. }
            | Self::SendInput { instance_id, .. }
            | Self::Remove { instance_id } => *instance_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectEnvelope {
    pub protocol_version: u16,
    pub operation_id: OperationId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub effect: ControlEffect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEffect {
    Spawn {
        agent_id: AgentId,
        transport: TransportKind,
    },
    Stop {
        force: bool,
    },
    WriteInput {
        input: PreparedInput,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservationEnvelope {
    pub protocol_version: u16,
    pub operation_id: Option<OperationId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub observation: ControlObservation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlObservation {
    Spawned {
        process_id: Option<u32>,
    },
    SpawnFailed {
        message: String,
    },
    ProcessExited {
        exit_code: Option<i32>,
    },
    StopCompleted {
        forced: bool,
    },
    InputCompleted,
    InputFailed {
        message: String,
    },
}

impl ControlObservation {
    pub fn requires_operation_id(&self) -> bool {
        !matches!(self, Self::ProcessExited { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited { exit_code: Option<i32> },
    Failed { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub instance_id: AgentInstanceId,
    pub agent_id: AgentId,
    pub transport: TransportKind,
    pub generation: SessionGeneration,
    pub status: SessionStatus,
    pub pending_operation: Option<OperationId>,
    pub pending_input: Option<PreparedInputKind>,
    pub process_id: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub protocol_version: u16,
    pub revision: u64,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub protocol_version: u16,
    pub sequence: u64,
    pub command_id: Option<CommandId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub event: ControlEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEventKind {
    Registered,
    StartRequested { operation_id: OperationId },
    Running { process_id: Option<u32> },
    StopRequested { operation_id: OperationId, force: bool },
    InputRequested {
        operation_id: OperationId,
        input_kind: PreparedInputKind,
    },
    InputCompleted { input_kind: PreparedInputKind },
    InputFailed {
        input_kind: PreparedInputKind,
        message: String,
    },
    Exited { exit_code: Option<i32>, forced: bool },
    Failed { message: String },
    Removed,
    ObservationIgnored { reason: ObservationIgnoredReason },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationIgnoredReason {
    UnsupportedProtocolVersion,
    UnknownInstance,
    StaleGeneration,
    MissingOperation,
    OperationMismatch,
    InvalidState,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlError {
    #[error("control protocol version {actual} is unsupported; expected {expected}")]
    UnsupportedProtocolVersion { expected: u16, actual: u16 },
    #[error("agent instance {instance_id:?} is already registered")]
    DuplicateInstance { instance_id: AgentInstanceId },
    #[error("agent instance {instance_id:?} is not registered")]
    UnknownInstance { instance_id: AgentInstanceId },
    #[error("agent instance {instance_id:?} already has pending operation {operation_id:?}")]
    OperationPending {
        instance_id: AgentInstanceId,
        operation_id: OperationId,
    },
    #[error("agent input was rejected: {error}")]
    InputRejected { error: InputPrepareError },
    #[error("agent instance {instance_id:?} cannot {action} while in state {status:?}")]
    InvalidTransition {
        instance_id: AgentInstanceId,
        action: String,
        status: SessionStatus,
    },
}

impl Default for ControlSnapshot {
    fn default() -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            revision: 0,
            sessions: Vec::new(),
        }
    }
}
