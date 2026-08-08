//! Reusable local wire client and authentication primitives for Gate4Agent nodes.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_secure_pipe;

#[cfg(windows)]
pub use windows::{
    auth_proof, local_hmac_sha256, negotiated_auth_proof, proofs_match, random_incarnation_id,
    random_nonce, AuthDirection, NamedPipeNodeClient, NodeClientError,
};
#[cfg(windows)]
pub type LocalNodeClient = NamedPipeNodeClient;
#[cfg(windows)]
pub use windows_secure_pipe::{
    connect_local_stream, LocalClientStream, LocalServerStream, OwnerOnlyLocalListener,
};
