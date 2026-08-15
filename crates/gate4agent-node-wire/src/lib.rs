//! Reusable local wire client and authentication primitives for Gate4Agent nodes.

mod auth;
mod client;
#[cfg(windows)]
mod windows_secure_pipe;
#[cfg(unix)]
mod unix_secure_socket;

pub use auth::{
    auth_proof, local_hmac_sha256, negotiated_auth_proof, proofs_match, random_incarnation_id,
    random_nonce, AuthDirection,
};
pub use client::{
    LocalNodeClient, LocalSessionHarnessMcpClient, LocalSessionHarnessMcpError, NodeClientError,
};
#[cfg(windows)]
pub type NamedPipeNodeClient = LocalNodeClient;
#[cfg(windows)]
pub use windows_secure_pipe::{
    connect_local_stream, LocalClientStream, LocalServerStream, OwnerOnlyLocalListener,
};
#[cfg(unix)]
pub use unix_secure_socket::{
    connect_local_stream, LocalClientStream, LocalServerStream, OwnerOnlyLocalListener,
};
