use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, ClientAuthentication,
    ClientFrame, ClientHello, ClientRole, FrameError, NodeEventEnvelope, NodeFailure, NodeHello,
    NodeId, NodeIncarnationId, NodeRequest, NodeResponse, RequestEnvelope, ServerFrame,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    NODE_AUTH_NONCE_BYTES, NODE_AUTH_PROOF_BYTES, NODE_INCARNATION_ID_BYTES,
    NODE_PROTOCOL_VERSION,
};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::ptr;
use std::time::Duration;
use thiserror::Error;
#[cfg(feature = "fixture")]
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::time::{sleep, timeout};

const PIPE_CONNECT_RETRIES: usize = 100;
const PIPE_CONNECT_RETRY_DELAY_MS: u64 = 20;
const AUTH_FRAME_TIMEOUT_MS: u64 = 5_000;
const FRAME_BODY_TIMEOUT_MS: u64 = 5_000;

pub struct NamedPipeNodeClient {
    pipe: NamedPipeClient,
    hello: NodeHello,
    next_request_id: u64,
    pending_events: VecDeque<NodeEventEnvelope>,
}

impl NamedPipeNodeClient {
    pub async fn connect(
        endpoint: &str,
        expected_node_id: &NodeId,
        role: ClientRole,
        access_token: &str,
    ) -> Result<Self, NodeClientError> {
        let mut pipe = connect_pipe(endpoint).await?;
        let client_nonce = random_nonce().map_err(NodeClientError::Authentication)?;
        write_json_frame_limited(
            &mut pipe,
            &ClientFrame::Hello(ClientHello::new(role, client_nonce)),
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await?;
        let challenge = timeout(
            Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
            read_json_frame_limited_body_timeout(
                &mut pipe,
                MAX_NODE_HELLO_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            ),
        )
        .await
        .map_err(|_| NodeClientError::AuthenticationTimedOut)??;
        let ServerFrame::Challenge(challenge) = challenge else {
            return Err(NodeClientError::Protocol(
                "server did not return an authentication challenge".to_owned(),
            ));
        };
        if challenge.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(NodeClientError::Protocol(
                "node protocol version mismatch".to_owned(),
            ));
        }
        let expected_server_proof = auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            role,
            &client_nonce,
            &challenge.server_nonce,
        )
        .map_err(NodeClientError::Authentication)?;
        if !proofs_match(&challenge.server_proof, &expected_server_proof) {
            return Err(NodeClientError::Protocol(
                "server failed access-token proof".to_owned(),
            ));
        }
        let client_proof = auth_proof(
            access_token.as_bytes(),
            AuthDirection::Client,
            role,
            &client_nonce,
            &challenge.server_nonce,
        )
        .map_err(NodeClientError::Authentication)?;
        write_json_frame_limited(
            &mut pipe,
            &ClientFrame::Authenticate(ClientAuthentication { client_proof }),
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await?;
        let server_hello = timeout(
            Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
            read_json_frame_limited_body_timeout(
                &mut pipe,
                MAX_NODE_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            ),
        )
        .await
        .map_err(|_| NodeClientError::AuthenticationTimedOut)??;
        let ServerFrame::Hello(hello) = server_hello else {
            return Err(NodeClientError::Protocol(
                "server did not return hello".to_owned(),
            ));
        };
        if hello.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(NodeClientError::Protocol(
                "node protocol version mismatch".to_owned(),
            ));
        }
        if &hello.snapshot.node_id != expected_node_id {
            return Err(NodeClientError::Protocol(format!(
                "node identity mismatch: expected '{}', received '{}'",
                expected_node_id,
                hello.snapshot.node_id,
            )));
        }
        Ok(Self {
            pipe,
            hello,
            next_request_id: 1,
            pending_events: VecDeque::new(),
        })
    }

    pub fn hello(&self) -> &NodeHello {
        &self.hello
    }

    pub async fn send(&mut self, request: NodeRequest) -> Result<u64, NodeClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(NodeClientError::RequestIdExhausted)?;
        write_json_frame_limited(
            &mut self.pipe,
            &ClientFrame::Request(RequestEnvelope {
                request_id,
                request,
            }),
            MAX_NODE_CLIENT_FRAME_BYTES,
        )
        .await?;
        Ok(request_id)
    }

    pub async fn recv(&mut self) -> Result<ServerFrame, NodeClientError> {
        Ok(read_json_frame_limited_body_timeout(
            &mut self.pipe,
            MAX_NODE_FRAME_BYTES,
            Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
        )
        .await?)
    }

    pub async fn request(&mut self, request: NodeRequest) -> Result<NodeResponse, NodeClientError> {
        let request_id = self.send(request).await?;
        loop {
            match self.recv().await? {
                ServerFrame::Reply(reply) if reply.request_id == request_id => {
                    return reply.result.map_err(NodeClientError::Node);
                }
                ServerFrame::Reply(reply) => {
                    return Err(NodeClientError::Protocol(format!(
                        "unexpected response id {} while waiting for {request_id}",
                        reply.request_id,
                    )));
                }
                ServerFrame::Event(event) => self.pending_events.push_back(event),
                ServerFrame::Challenge(_) => {
                    return Err(NodeClientError::Protocol(
                        "duplicate server challenge".to_owned(),
                    ));
                }
                ServerFrame::Hello(_) => {
                    return Err(NodeClientError::Protocol(
                        "duplicate server hello".to_owned(),
                    ));
                }
            }
        }
    }

    pub fn take_event(&mut self) -> Option<NodeEventEnvelope> {
        self.pending_events.pop_front()
    }

    #[cfg(feature = "fixture")]
    pub async fn send_malformed_json_frame_for_fixture(&mut self) -> Result<(), NodeClientError> {
        self.pipe.write_u32_le(1).await?;
        self.pipe.write_all(b"{").await?;
        self.pipe.flush().await?;
        Ok(())
    }
}

async fn connect_pipe(endpoint: &str) -> io::Result<NamedPipeClient> {
    let mut last_error = None;
    for _ in 0..PIPE_CONNECT_RETRIES {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(error) => {
                let retryable = matches!(error.kind(), io::ErrorKind::NotFound)
                    || error.raw_os_error() == Some(231);
                if !retryable {
                    return Err(error);
                }
                last_error = Some(error);
                sleep(Duration::from_millis(PIPE_CONNECT_RETRY_DELAY_MS)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "named pipe was not available")
    }))
}

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

pub fn auth_proof(
    access_token: &[u8],
    direction: AuthDirection,
    role: ClientRole,
    client_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
    server_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
) -> Result<[u8; NODE_AUTH_PROOF_BYTES], String> {
    let mut message = Vec::with_capacity(32 + (NODE_AUTH_NONCE_BYTES * 2));
    message.extend_from_slice(b"gate4agent-node-auth-v3\0");
    message.extend_from_slice(&NODE_PROTOCOL_VERSION.to_le_bytes());
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
    hmac_sha256(access_token, &message)
}

fn hmac_sha256(
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

struct AlgorithmHandle(*mut c_void);

impl Drop for AlgorithmHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptCloseAlgorithmProvider(self.0, 0);
        }
    }
}

struct HashHandle(*mut c_void);

impl Drop for HashHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptDestroyHash(self.0);
        }
    }
}

const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
const BCRYPT_ALG_HANDLE_HMAC_FLAG: u32 = 0x0000_0008;
const BCRYPT_SHA256_ALGORITHM: [u16; 7] = [83, 72, 65, 50, 53, 54, 0];
const BCRYPT_OBJECT_LENGTH: [u16; 13] = [79, 98, 106, 101, 99, 116, 76, 101, 110, 103, 116, 104, 0];

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

#[derive(Debug, Error)]
pub enum NodeClientError {
    #[error("named pipe I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("node rejected request: {0:?}")]
    Node(NodeFailure),
    #[error("node protocol failed: {0}")]
    Protocol(String),
    #[error("node authentication frame was not received before the bounded deadline")]
    AuthenticationTimedOut,
    #[error("node authentication primitive failed: {0}")]
    Authentication(String),
    #[error("request id counter is exhausted")]
    RequestIdExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_cng_hmac_sha256_matches_the_standard_vector() {
        let actual = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog").unwrap();
        let expected = [
            0xf7, 0xbc, 0x83, 0xf4, 0x30, 0x53, 0x84, 0x24,
            0xb1, 0x32, 0x98, 0xe6, 0xaa, 0x6f, 0xb1, 0x43,
            0xef, 0x4d, 0x59, 0xa1, 0x49, 0x46, 0x17, 0x59,
            0x97, 0x47, 0x9d, 0xbc, 0x2d, 0x1a, 0x3c, 0xd8,
        ];
        assert_eq!(actual, expected);
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

    #[test]
    fn windows_cng_generates_a_bounded_incarnation_id() {
        let incarnation_id = random_incarnation_id().unwrap();
        let encoded = incarnation_id.to_string();
        assert_eq!(encoded.len(), NODE_INCARNATION_ID_BYTES * 2);
        assert_eq!(encoded.parse::<NodeIncarnationId>().unwrap(), incarnation_id);
    }
}
