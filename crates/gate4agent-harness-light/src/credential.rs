//! In-process operator credential minting and verification.
//!
//! `gate4agent-harness-service`'s own operator credential check
//! (`HarnessOperatorCredentialAuthority` in `runtime.rs`) is a private,
//! non-`pub` type, and is bound to an *externally supplied* credential (the
//! full harness reads `GATE4AGENT_HARNESS_OPERATOR_TOKEN` from the
//! environment -- see `gate4agent-harness-service/src/bin/gate4agent-
//! harness.rs`). Light mode's contract is the opposite: the credential is
//! generated fresh in-process at `start_harness_light` and handed back to
//! the caller, never read from the environment. This module is therefore a
//! deliberate, small light-local reimplementation, not a promotion --
//! rehosting the private type would not have fit `start_harness_light`'s
//! signature anyway. It mirrors that type's own technique exactly (reduce
//! the credential to an HMAC digest, compare digests in constant time via
//! `proofs_match`), just generated rather than externally supplied.

use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_node_wire::{local_hmac_sha256, proofs_match, random_nonce};

use crate::error::CredentialMintError;
use crate::util::encode_hex;

const OPERATOR_TOKEN_PREFIX: &str = "g4aho_";
const CREDENTIAL_DIGEST_DOMAIN: &[u8] = b"gate4agent-harness-light-operator-credential-v1\0";

/// Holds only the minted credential's HMAC digest, never the plaintext
/// secret a second time -- the plaintext lives exactly once, in the
/// `HarnessOperatorCredential` returned alongside this authority.
pub(crate) struct LightCredentialAuthority {
    digest: [u8; 32],
}

impl LightCredentialAuthority {
    /// Generates a fresh `g4aho_` + 64 lowercase-hex operator credential
    /// (the same shape `HarnessOperatorCredential::parse` requires) from a
    /// cryptographically random 32-byte secret, and returns an authority
    /// that recognizes exactly that one credential.
    pub(crate) fn mint() -> Result<(Self, HarnessOperatorCredential), CredentialMintError> {
        let secret = random_nonce().map_err(CredentialMintError::Crypto)?;
        let credential = HarnessOperatorCredential::parse(format!(
            "{OPERATOR_TOKEN_PREFIX}{}",
            encode_hex(&secret),
        ))?;
        let digest = local_hmac_sha256(CREDENTIAL_DIGEST_DOMAIN, credential.expose().as_bytes())
            .map_err(CredentialMintError::Crypto)?;
        Ok((Self { digest }, credential))
    }

    /// Constant-time membership check against the one credential this
    /// authority was minted for.
    pub(crate) fn verify(&self, credential: &HarnessOperatorCredential) -> bool {
        match local_hmac_sha256(CREDENTIAL_DIGEST_DOMAIN, credential.expose().as_bytes()) {
            Ok(actual) => proofs_match(&actual, &self.digest),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_credential_is_recognized_and_others_are_rejected() {
        let (authority, credential) = LightCredentialAuthority::mint().unwrap();
        assert!(credential.expose().starts_with(OPERATOR_TOKEN_PREFIX));
        assert!(authority.verify(&credential));

        let (_, other_credential) = LightCredentialAuthority::mint().unwrap();
        assert_ne!(credential.expose(), other_credential.expose());
        assert!(!authority.verify(&other_credential));
    }
}
