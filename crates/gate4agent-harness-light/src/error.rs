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
use gate4agent_harness_service::c2::HarnessC2Error;
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
    /// The physical connection to this process's own dedicated c2 is being
    /// re-established after a loss -- see `crate::c2::resolve_exact_route`'s
    /// doc comment for why this must be checked before, not derived from,
    /// the (possibly stale-but-populated) cached topology.
    #[error("c2 relay is reconnecting")]
    RelayReconnecting,
    #[error("no advertised spawn profile matches the requested provider profile")]
    SpawnProfileUnavailable,
    #[error("credential/nonce cryptography failed: {0}")]
    Crypto(String),
    /// A relay call did not settle inside its family's per-request deadline
    /// bucket (`crate::relay::deadline` -- mirrors the full harness's own
    /// `HOST_*_RESPONSE_DEADLINE` constants). A hung node call must not hang
    /// the connection forever; see `crate::relay`'s module doc comment.
    #[error("the request exceeded its response deadline")]
    Deadline,
    /// A reused `gate4agent_harness_service::c2` correlate/project function
    /// (node-workspace read/write, native history, session-record mutation,
    /// resource mutation) rejected the node's own response: either it did not
    /// match what was asked (`*CorrelationMismatch`), it failed re-validation
    /// after projection (`*Projection`), or -- read-family only -- the file
    /// content exceeded the wire's size cap (`NodeWorkspaceReadTooLarge`).
    /// Carries the original `HarnessC2Error` for logging.
    #[error("response projection rejected: {0:?}")]
    Projection(HarnessC2Error),
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
            Self::NodeOffline | Self::MissingIncarnation | Self::RelayReconnecting => {
                HarnessOperatorHostErrorV1::Unavailable
            }
            Self::IncarnationChanged => HarnessOperatorHostErrorV1::Conflict,
            Self::UnexpectedResponse | Self::Crypto(_) => HarnessOperatorHostErrorV1::Internal,
            Self::NodeRejected(code) => map_node_failure(*code),
            Self::Deadline => HarnessOperatorHostErrorV1::Deadline,
            // `NodeWorkspaceReadTooLarge` is the only member of this family
            // that is not a bare correlation/re-validation failure; every
            // other `HarnessC2Error` a reused correlate/project function can
            // possibly return (`*CorrelationMismatch`/`*Projection`) means
            // the node's own response did not match its own request or
            // failed re-validation after projection -- a host-side bug, not
            // anything the operator caused, so `Internal`.
            Self::Projection(HarnessC2Error::NodeWorkspaceReadTooLarge) => {
                HarnessOperatorHostErrorV1::TooLarge
            }
            Self::Projection(_) => HarnessOperatorHostErrorV1::Internal,
        }
    }
}

/// One shared `NodeFailureCode` -> `HarnessOperatorHostErrorV1` taxonomy
/// across every verb family this crate relays (session spawn/control,
/// node-workspace read/write, native history, session-record mutation,
/// resource mutation, host-directory browse) -- unlike
/// `gate4agent-harness-service::runtime`, which keeps one bespoke `map_*_error`
/// per family (six of them) because each family's `HarnessC2Error` wraps a
/// distinct set of enqueue/transport/deadline/route-mismatch variants around
/// the shared `NodeFailureCode`. Light mode's direct-relay model (no
/// `Prepared`/`Pending` C2-waiter split, see `crate::relay`'s module doc
/// comment) has no such per-family error wrapper to key off, so this is
/// deliberately the union of the full harness's six tables, bucketed by the
/// same invalid-shape/unknown-target/already-conflicting/contended/timed-out/
/// unsupported meaning every one of them already uses. Two codes are
/// genuinely ambiguous across families in the full harness's own tables
/// (`UnknownWorkspace`: `NotFound` in read/write/resource/session-record,
/// `Unavailable` only in the native-history pool; `NotGitRepository`:
/// `Conflict` in read/write, `NotFound` in resource) -- this union picks the
/// majority mapping for each (`NotFound` for both) rather than threading verb
/// identity through this shared function for two edge cases. Every mapping
/// remains a real, typed `HarnessOperatorHostErrorV1`, per the app-harness
/// protocol contract's typed-rejection principle -- only the granularity is
/// coarser than the full harness's own per-family tables in these two spots.
fn map_node_failure(code: NodeFailureCode) -> HarnessOperatorHostErrorV1 {
    match code {
        NodeFailureCode::InvalidRequest
        | NodeFailureCode::InvalidRepositoryPath
        | NodeFailureCode::InvalidWorkspaceRoot
        | NodeFailureCode::HostDirectoryInvalid => HarnessOperatorHostErrorV1::InvalidRequest,
        NodeFailureCode::UnknownWorkspace
        | NodeFailureCode::RepositoryFileNotFound
        | NodeFailureCode::RepositoryParentNotFound
        | NodeFailureCode::UnknownSessionRecord
        | NodeFailureCode::UnknownSession
        | NodeFailureCode::UnknownContextPack
        | NodeFailureCode::NotGitRepository => HarnessOperatorHostErrorV1::NotFound,
        NodeFailureCode::SpawnProfileRevisionMismatch
        | NodeFailureCode::BindingMismatch
        | NodeFailureCode::StaleGeneration
        // The CAS conflict: a stale `expected_revision` on `WriteNodeWorkspaceFile`
        // must surface as a typed `Conflict`, never a generic failure -- see
        // the app-harness protocol contract's mutation-discipline principle.
        | NodeFailureCode::RepositoryFileRevisionConflict
        | NodeFailureCode::RepositoryFileNotRegular
        | NodeFailureCode::RepositoryPathUnsafe
        | NodeFailureCode::RepositoryEntryAlreadyExists
        | NodeFailureCode::RepositoryParentNotDirectory
        | NodeFailureCode::SessionRecordConflict
        | NodeFailureCode::SessionRecordNotResumable
        | NodeFailureCode::SessionWorkspaceMismatch
        | NodeFailureCode::WorkspaceRegistrationRequired
        | NodeFailureCode::StaleNativeSessionCatalog
        | NodeFailureCode::DuplicateWorkspaceId
        | NodeFailureCode::DuplicateWorkspaceRoot
        | NodeFailureCode::LastWorkspace
        | NodeFailureCode::WorktreeConflict
        | NodeFailureCode::WorktreeProtected
        | NodeFailureCode::WorktreeDirty
        | NodeFailureCode::WorktreeLocked
        | NodeFailureCode::StandaloneWorkspaceRecoveryRequired
        | NodeFailureCode::ManagedWorktreeRecoveryRequired => HarnessOperatorHostErrorV1::Conflict,
        NodeFailureCode::ControllerBusy
        | NodeFailureCode::WorkspaceBusy
        | NodeFailureCode::BackendBusy
        | NodeFailureCode::SessionRecordBusy
        | NodeFailureCode::ControllerRequired
        | NodeFailureCode::ContextPackBusy => HarnessOperatorHostErrorV1::Busy,
        NodeFailureCode::SpawnDeadlineExceeded
        | NodeFailureCode::RepositoryFileReadTimedOut
        | NodeFailureCode::GitReadTimedOut
        | NodeFailureCode::HostDirectoryReadTimedOut
        | NodeFailureCode::RepositoryFileWriteTimedOut
        | NodeFailureCode::RepositoryEntryCreateTimedOut => HarnessOperatorHostErrorV1::Deadline,
        NodeFailureCode::ResponseTooLarge => HarnessOperatorHostErrorV1::TooLarge,
        NodeFailureCode::UnsupportedCapability
        | NodeFailureCode::BackendDisconnected
        | NodeFailureCode::BackendOperationFailed
        | NodeFailureCode::ShuttingDown
        | NodeFailureCode::RepositoryFileReadFailed
        | NodeFailureCode::GitReadFailed
        | NodeFailureCode::RepositoryFileWriteFailed
        | NodeFailureCode::RepositoryEntryCreateFailed
        | NodeFailureCode::HostDirectoryReadFailed
        | NodeFailureCode::ContextPackMaterializationFailed => {
            HarnessOperatorHostErrorV1::Unavailable
        }
        NodeFailureCode::Unauthorized => HarnessOperatorHostErrorV1::Unauthorized,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}
