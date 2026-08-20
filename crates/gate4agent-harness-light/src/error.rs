//! Error types for `gate4agent-harness-light`.
//!
//! Two tiers: [`HarnessLightError`] is the crate's public, top-level error
//! (start-up and shutdown failures only -- nothing per-request ever reaches
//! a caller of `start_harness_light`, see the module doc on `dispatch`).
//! [`LightRelayError`] is the crate-private error every C2/Node relay call
//! (`crate::c2`, `crate::relay`) produces; [`LightRelayError::into_host_error`]
//! is this crate's light-local mirror of `gate4agent-harness-service`'s own
//! `map_session_spawn_node_failure`/`map_session_control_error` (private to
//! that crate, so not reusable here -- see the crate-level report for why
//! this is a deliberate, documented light-local reimplementation rather than
//! a promotion).

use gate4agent_c2_client::C2ControlError;
use gate4agent_harness_api::{HarnessOperatorApiError, HarnessOperatorHostErrorV1};
use gate4agent_node_protocol::NodeFailureCode;
use thiserror::Error;

/// Top-level error `start_harness_light`/`HarnessLightRunning::shutdown` can
/// return. Never constructed per-request -- every operator request always
/// gets a typed `HarnessOperatorReplyV1`, logged, and the connection closes
/// normally; see `crate::dispatch`.
#[derive(Debug, Error)]
pub enum HarnessLightError {
    #[error("harness-light failed to connect to c2: {0}")]
    C2Connect(C2ControlError),
    #[error("harness-light failed to mint the operator credential: {0}")]
    Credential(#[from] CredentialMintError),
    #[error("harness-light failed to bind the operator endpoint: {0}")]
    Bind(std::io::Error),
    #[error("harness-light host task ended unexpectedly: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Failure minting the in-process operator credential (`crate::credential`).
#[derive(Debug, Error)]
pub enum CredentialMintError {
    #[error("credential cryptography failed: {0}")]
    Crypto(String),
    #[error(transparent)]
    Api(#[from] HarnessOperatorApiError),
}

/// Crate-private error for one C2/Node relay attempt: route resolution
/// (`crate::c2::exact_route`), a snapshot fetch, a spawn, or one of the eight
/// session-control verbs. Every constructor site logs before converting this
/// into the typed `HarnessOperatorHostErrorV1` the operator wire carries.
#[derive(Debug, Error)]
pub(crate) enum LightRelayError {
    #[error("request failed local validation")]
    InvalidRequest,
    #[error("c2 control request failed: {0}")]
    Transport(#[from] C2ControlError),
    #[error("node rejected the request: {0:?}")]
    NodeRejected(NodeFailureCode),
    #[error("c2 returned an unexpected response shape for this request")]
    UnexpectedResponse,
    #[error("the node's route incarnation changed mid-request")]
    IncarnationChanged,
    #[error("node is not known to c2")]
    UnknownNode,
    #[error("node is not currently online")]
    NodeOffline,
    #[error("node has no current incarnation")]
    MissingIncarnation,
    #[error("no advertised spawn profile matches the requested provider profile")]
    SpawnProfileUnavailable,
    #[error("credential/nonce cryptography failed: {0}")]
    Crypto(String),
}

impl LightRelayError {
    /// Maps this internal error to the wire-visible
    /// `HarnessOperatorHostErrorV1`, mirroring the taxonomy
    /// `gate4agent-harness-service::runtime`'s (private)
    /// `map_session_spawn_node_failure`/`map_session_control_error` already
    /// establish for the same underlying `NodeFailureCode`/transport
    /// failures, so a given node-side rejection reads the same way through
    /// either harness.
    pub(crate) fn into_host_error(&self) -> HarnessOperatorHostErrorV1 {
        match self {
            Self::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            Self::Transport(C2ControlError::QueueFull) => HarnessOperatorHostErrorV1::Busy,
            Self::Transport(_) => HarnessOperatorHostErrorV1::Unavailable,
            Self::UnknownNode | Self::SpawnProfileUnavailable => {
                HarnessOperatorHostErrorV1::NotFound
            }
            Self::NodeOffline | Self::MissingIncarnation => {
                HarnessOperatorHostErrorV1::Unavailable
            }
            Self::IncarnationChanged => HarnessOperatorHostErrorV1::Conflict,
            Self::UnexpectedResponse | Self::Crypto(_) => HarnessOperatorHostErrorV1::Internal,
            Self::NodeRejected(code) => map_node_failure(*code),
        }
    }
}

fn map_node_failure(code: NodeFailureCode) -> HarnessOperatorHostErrorV1 {
    match code {
        NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        NodeFailureCode::UnknownWorkspace => HarnessOperatorHostErrorV1::NotFound,
        NodeFailureCode::SpawnProfileRevisionMismatch
        | NodeFailureCode::BindingMismatch
        | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
        NodeFailureCode::ControllerBusy
        | NodeFailureCode::WorkspaceBusy
        | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
        NodeFailureCode::SpawnDeadlineExceeded => HarnessOperatorHostErrorV1::Deadline,
        NodeFailureCode::UnsupportedCapability
        | NodeFailureCode::BackendDisconnected
        | NodeFailureCode::BackendOperationFailed
        | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
        NodeFailureCode::Unauthorized => HarnessOperatorHostErrorV1::Unauthorized,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}
