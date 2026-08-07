//! Reusable local wire client and authentication primitives for Gate4Agent nodes.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    auth_proof, local_hmac_sha256, proofs_match, random_incarnation_id, random_nonce, AuthDirection,
    NamedPipeNodeClient, NodeClientError,
};
