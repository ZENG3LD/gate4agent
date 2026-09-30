use gate4agent_node_protocol::{
    encode_node_compatibility_auth_binding, ClientCompatibilityOffer, ClientRole,
    NegotiatedNodeCompatibility, NodeIncarnationId, BUILD_STAMP, NODE_AUTH_NONCE_BYTES,
    NODE_AUTH_PROOF_BYTES, NODE_INCARNATION_ID_BYTES,
};

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::ptr;

#[derive(Clone, Copy)]
pub enum AuthDirection {
    Server,
    Client,
}

pub fn random_nonce() -> Result<[u8; NODE_AUTH_NONCE_BYTES], String> {
    let mut nonce = [0; NODE_AUTH_NONCE_BYTES];
    fill_random(&mut nonce)?;
    Ok(nonce)
}

pub fn random_incarnation_id() -> Result<NodeIncarnationId, String> {
    let mut bytes = [0; NODE_INCARNATION_ID_BYTES];
    fill_random(&mut bytes)?;
    Ok(NodeIncarnationId::from_bytes(bytes))
}

#[cfg(windows)]
fn fill_random(bytes: &mut [u8]) -> Result<(), String> {
    let status = unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    cng_status("BCryptGenRandom", status)?;
    Ok(())
}

#[cfg(unix)]
fn fill_random(bytes: &mut [u8]) -> Result<(), String> {
    use ring::rand::SecureRandom;

    ring::rand::SystemRandom::new()
        .fill(bytes)
        .map_err(|_| "ring SystemRandom failed".to_owned())
}

pub fn auth_proof(
    access_token: &[u8],
    direction: AuthDirection,
    role: ClientRole,
    client_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
    server_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
) -> Result<[u8; NODE_AUTH_PROOF_BYTES], String> {
    let mut message = Vec::with_capacity(32 + (NODE_AUTH_NONCE_BYTES * 2) + BUILD_STAMP.len());
    message.extend_from_slice(b"gate4agent-node-auth-v3\0");
    message.extend_from_slice(&(BUILD_STAMP.len() as u16).to_le_bytes());
    message.extend_from_slice(BUILD_STAMP.as_bytes());
    message.push(match direction {
        AuthDirection::Server => 1,
        AuthDirection::Client => 2,
    });
    message.push(match role {
        ClientRole::Operator => 1,
        ClientRole::Observer => 2,
    });
    message.extend_from_slice(client_nonce);
    message.extend_from_slice(server_nonce);
    local_hmac_sha256(access_token, &message)
}

pub fn negotiated_auth_proof(
    access_token: &[u8],
    direction: AuthDirection,
    role: ClientRole,
    client_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
    server_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
    offer: &ClientCompatibilityOffer,
    selected: &NegotiatedNodeCompatibility,
) -> Result<[u8; NODE_AUTH_PROOF_BYTES], String> {
    let binding = encode_node_compatibility_auth_binding(offer, selected)
        .map_err(|error| error.to_string())?;
    let binding_length = u32::try_from(binding.len())
        .map_err(|_| "node compatibility authentication binding is too large".to_owned())?;
    let mut message = Vec::with_capacity(
        48 + (NODE_AUTH_NONCE_BYTES * 2) + BUILD_STAMP.len() + binding.len(),
    );
    message.extend_from_slice(b"gate4agent-node-auth-negotiated-v1\0");
    message.extend_from_slice(&(BUILD_STAMP.len() as u16).to_le_bytes());
    message.extend_from_slice(BUILD_STAMP.as_bytes());
    message.push(match direction {
        AuthDirection::Server => 1,
        AuthDirection::Client => 2,
    });
    message.push(match role {
        ClientRole::Operator => 1,
        ClientRole::Observer => 2,
    });
    message.extend_from_slice(client_nonce);
    message.extend_from_slice(server_nonce);
    message.extend_from_slice(&binding_length.to_le_bytes());
    message.extend_from_slice(&binding);
    local_hmac_sha256(access_token, &message)
}

#[cfg(windows)]
pub fn local_hmac_sha256(
    secret: &[u8],
    message: &[u8],
) -> Result<[u8; NODE_AUTH_PROOF_BYTES], String> {
    let mut algorithm = ptr::null_mut();
    cng_status(
        "BCryptOpenAlgorithmProvider",
        unsafe {
            BCryptOpenAlgorithmProvider(
                &mut algorithm,
                BCRYPT_SHA256_ALGORITHM.as_ptr(),
                ptr::null(),
                BCRYPT_ALG_HANDLE_HMAC_FLAG,
            )
        },
    )?;
    let algorithm = AlgorithmHandle(algorithm);

    let mut object_length = 0_u32;
    let mut copied = 0_u32;
    cng_status(
        "BCryptGetProperty(ObjectLength)",
        unsafe {
            BCryptGetProperty(
                algorithm.0,
                BCRYPT_OBJECT_LENGTH.as_ptr(),
                (&mut object_length as *mut u32).cast::<u8>(),
                std::mem::size_of::<u32>() as u32,
                &mut copied,
                0,
            )
        },
    )?;
    if copied != std::mem::size_of::<u32>() as u32 || object_length == 0 {
        return Err("BCryptGetProperty(ObjectLength) returned an invalid length".to_owned());
    }
    let mut object = vec![0_u8; object_length as usize];
    let mut hash = ptr::null_mut();
    cng_status(
        "BCryptCreateHash",
        unsafe {
            BCryptCreateHash(
                algorithm.0,
                &mut hash,
                object.as_mut_ptr(),
                object.len() as u32,
                secret.as_ptr().cast_mut(),
                secret.len() as u32,
                0,
            )
        },
    )?;
    let hash = HashHandle(hash);
    cng_status(
        "BCryptHashData",
        unsafe {
            BCryptHashData(
                hash.0,
                message.as_ptr().cast_mut(),
                message.len() as u32,
                0,
            )
        },
    )?;
    let mut proof = [0_u8; NODE_AUTH_PROOF_BYTES];
    cng_status(
        "BCryptFinishHash",
        unsafe { BCryptFinishHash(hash.0, proof.as_mut_ptr(), proof.len() as u32, 0) },
    )?;
    Ok(proof)
}

#[cfg(unix)]
pub fn local_hmac_sha256(
    secret: &[u8],
    message: &[u8],
) -> Result<[u8; NODE_AUTH_PROOF_BYTES], String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::sign(&key, message)
        .as_ref()
        .try_into()
        .map_err(|_| "ring HMAC-SHA256 returned an invalid proof length".to_owned())
}

pub fn proofs_match(
    actual: &[u8; NODE_AUTH_PROOF_BYTES],
    expected: &[u8; NODE_AUTH_PROOF_BYTES],
) -> bool {
    actual
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(windows)]
fn cng_status(operation: &str, status: i32) -> Result<(), String> {
    if status >= 0 {
        Ok(())
    } else {
        Err(format!(
            "{operation} failed with NTSTATUS 0x{:08x}",
            status as u32,
        ))
    }
}

#[cfg(windows)]
struct AlgorithmHandle(*mut c_void);

#[cfg(windows)]
impl Drop for AlgorithmHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptCloseAlgorithmProvider(self.0, 0);
        }
    }
}

#[cfg(windows)]
struct HashHandle(*mut c_void);

#[cfg(windows)]
impl Drop for HashHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptDestroyHash(self.0);
        }
    }
}

#[cfg(windows)]
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
#[cfg(windows)]
const BCRYPT_ALG_HANDLE_HMAC_FLAG: u32 = 0x0000_0008;
#[cfg(windows)]
const BCRYPT_SHA256_ALGORITHM: [u16; 7] = [83, 72, 65, 50, 53, 54, 0];
#[cfg(windows)]
const BCRYPT_OBJECT_LENGTH: [u16; 13] = [79, 98, 106, 101, 99, 116, 76, 101, 110, 103, 116, 104, 0];

#[cfg(windows)]
#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(
        algorithm: *mut c_void,
        buffer: *mut u8,
        buffer_length: u32,
        flags: u32,
    ) -> i32;
    fn BCryptOpenAlgorithmProvider(
        algorithm: *mut *mut c_void,
        algorithm_id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    fn BCryptGetProperty(
        object: *mut c_void,
        property: *const u16,
        output: *mut u8,
        output_length: u32,
        result_length: *mut u32,
        flags: u32,
    ) -> i32;
    fn BCryptCreateHash(
        algorithm: *mut c_void,
        hash: *mut *mut c_void,
        hash_object: *mut u8,
        hash_object_length: u32,
        secret: *mut u8,
        secret_length: u32,
        flags: u32,
    ) -> i32;
    fn BCryptHashData(hash: *mut c_void, input: *mut u8, input_length: u32, flags: u32) -> i32;
    fn BCryptFinishHash(hash: *mut c_void, output: *mut u8, output_length: u32, flags: u32) -> i32;
    fn BCryptDestroyHash(hash: *mut c_void) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        ArchitectureId, CapabilityId, HostDescriptor, LocalTransportKind,
        NodeCompatibilitySupport, OperatingSystemId, PathEncoding, PathSemantics,
        PathStyle, ProtocolRange, StateSchemaSupport,
        NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_PROVIDER_ID_OPEN_CAPABILITY,
    };

    fn negotiated_fixture() -> (ClientCompatibilityOffer, NegotiatedNodeCompatibility) {
        let offer = ClientCompatibilityOffer {
            build_stamp: BUILD_STAMP.to_owned(),
            capabilities: vec![CapabilityId::new(
                NODE_COMPATIBILITY_METADATA_CAPABILITY,
            )
            .unwrap()],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::exact(1).unwrap(),
            }),
        };
        let support = NodeCompatibilitySupport {
            build_stamp: BUILD_STAMP.to_owned(),
            capabilities: offer.capabilities.clone(),
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
            local_transport: LocalTransportKind::WindowsNamedPipe,
            state_schema: StateSchemaSupport {
                versions: ProtocolRange::exact(1).unwrap(),
            },
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
        };
        let selected = support.negotiate(&offer).unwrap();
        (offer, selected)
    }

    #[test]
    fn hmac_sha256_matches_the_standard_vector() {
        let actual = local_hmac_sha256(
            b"key",
            b"The quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        assert_eq!(
            actual,
            [
                0xf7, 0xbc, 0x83, 0xf4, 0x30, 0x53, 0x84, 0x24,
                0xb1, 0x32, 0x98, 0xe6, 0xaa, 0x6f, 0xb1, 0x43,
                0xef, 0x4d, 0x59, 0xa1, 0x49, 0x46, 0x17, 0x59,
                0x97, 0x47, 0x9d, 0xbc, 0x2d, 0x1a, 0x3c, 0xd8,
            ],
        );
    }

    /// Pins the MESSAGE the proof is an HMAC of, field by field, instead of
    /// pinning the proof itself.
    ///
    /// It used to pin the proof, and could not have worked: `auth_proof`
    /// mixes `BUILD_STAMP` into that message on purpose, so a peer built
    /// from another tree cannot authenticate. A frozen output is therefore
    /// only valid for one build, and every rebuild moves the stamp -- red on
    /// every build, which is exactly how a test stops being read.
    ///
    /// What must not drift is the layout: the domain tag, the build stamp
    /// length prefix and its ASCII bytes, the direction and role bytes, and
    /// the two nonces in that order. Reorder any of it, drop the stamp, or
    /// collide the direction/role encoding and this fails; rebuild the tree
    /// and it still passes, because the expectation is built from the same
    /// stamp the code uses.
    #[test]
    fn the_legacy_auth_proof_is_an_hmac_over_exactly_this_message() {
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let mut expected_message = Vec::new();
        expected_message.extend_from_slice(b"gate4agent-node-auth-v3\0");
        expected_message.extend_from_slice(&(BUILD_STAMP.len() as u16).to_le_bytes());
        expected_message.extend_from_slice(BUILD_STAMP.as_bytes());
        expected_message.push(1); // AuthDirection::Server
        expected_message.push(1); // ClientRole::Operator
        expected_message.extend_from_slice(&client_nonce);
        expected_message.extend_from_slice(&server_nonce);

        assert_eq!(
            auth_proof(
                b"local-secret",
                AuthDirection::Server,
                ClientRole::Operator,
                &client_nonce,
                &server_nonce,
            )
            .unwrap(),
            local_hmac_sha256(b"local-secret", &expected_message).unwrap(),
        );
    }

    #[test]
    fn mutual_auth_proofs_are_direction_and_role_bound() {
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let server = auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
        )
        .unwrap();
        let client = auth_proof(
            b"local-secret",
            AuthDirection::Client,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
        )
        .unwrap();
        let observer = auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Observer,
            &client_nonce,
            &server_nonce,
        )
        .unwrap();
        assert!(!proofs_match(&server, &client));
        assert!(!proofs_match(&server, &observer));
        assert!(proofs_match(&server, &server));
    }

    /// Same correction as its legacy sibling, plus the part that was
    /// always right: the negotiated proof is bound to the offer AND to the
    /// selection, so neither can be tampered with between the two ends.
    ///
    /// The layout here carries one thing the legacy message does not -- a
    /// length prefix ahead of the compatibility binding -- and that prefix
    /// is what stops a crafted offer/selection pair from shifting bytes
    /// across the boundary into the nonces. Pinning the layout keeps it
    /// checked; pinning the output never did, because the build stamp
    /// inside it moves on every rebuild by design.
    #[test]
    fn the_negotiated_auth_proof_is_an_hmac_over_exactly_this_message_and_is_bound() {
        let (offer, selected) = negotiated_fixture();
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let proof = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();

        let binding = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        let mut expected_message = Vec::new();
        expected_message.extend_from_slice(b"gate4agent-node-auth-negotiated-v1\0");
        expected_message.extend_from_slice(&(BUILD_STAMP.len() as u16).to_le_bytes());
        expected_message.extend_from_slice(BUILD_STAMP.as_bytes());
        expected_message.push(1); // AuthDirection::Server
        expected_message.push(1); // ClientRole::Operator
        expected_message.extend_from_slice(&client_nonce);
        expected_message.extend_from_slice(&server_nonce);
        expected_message
            .extend_from_slice(&u32::try_from(binding.len()).unwrap().to_le_bytes());
        expected_message.extend_from_slice(&binding);
        assert_eq!(
            proof,
            local_hmac_sha256(b"local-secret", &expected_message).unwrap(),
        );

        // The two domains must not collide: a legacy proof over the same
        // token, direction, role and nonces is a different value, so a
        // peer cannot replay one handshake's proof into the other.
        assert!(!proofs_match(
            &proof,
            &auth_proof(
                b"local-secret",
                AuthDirection::Server,
                ClientRole::Operator,
                &client_nonce,
                &server_nonce,
            )
            .unwrap(),
        ));

        let mut tampered_offer = offer.clone();
        tampered_offer.capabilities.clear();
        let offer_proof = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &tampered_offer,
            &selected,
        )
        .unwrap();
        assert!(!proofs_match(&proof, &offer_proof));

        let mut tampered_selection = selected.clone();
        tampered_selection.state_schema_version = None;
        let selection_proof = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &tampered_selection,
        )
        .unwrap();
        assert!(!proofs_match(&proof, &selection_proof));
    }

    #[test]
    fn open_provider_capability_is_bound_in_both_offer_and_selection() {
        let (mut offer, mut selected) = negotiated_fixture();
        let capability = CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap();
        offer.capabilities.push(capability.clone());
        selected.capabilities.push(capability);
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let proof = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();

        let mut legacy_offer = offer.clone();
        legacy_offer.capabilities.retain(|candidate| {
            candidate.as_str() != NODE_PROVIDER_ID_OPEN_CAPABILITY
        });
        let changed_offer = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &legacy_offer,
            &selected,
        )
        .unwrap();
        assert!(!proofs_match(&proof, &changed_offer));

        let mut legacy_selection = selected.clone();
        legacy_selection.capabilities.retain(|candidate| {
            candidate.as_str() != NODE_PROVIDER_ID_OPEN_CAPABILITY
        });
        let changed_selection = negotiated_auth_proof(
            b"local-secret",
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &legacy_selection,
        )
        .unwrap();
        assert!(!proofs_match(&proof, &changed_selection));
    }

    #[test]
    fn random_incarnation_id_is_bounded() {
        let incarnation_id = random_incarnation_id().unwrap();
        let encoded = incarnation_id.to_string();
        assert_eq!(encoded.len(), NODE_INCARNATION_ID_BYTES * 2);
        assert_eq!(encoded.parse::<NodeIncarnationId>().unwrap(), incarnation_id);
    }
}
