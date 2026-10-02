//! Tip 6: browser-bridge TCP reachability over the tip-5 underlay path.
//!
//! Application dialect stays HTTP+WS over TCP. This module only carries
//! length-framed TCP bytes inside the encrypted UDP underlay session after
//! the **underlay auth token** barrier passes. It does not invent a second
//! observe/drive dialect and is not a WireGuard daemon.
//!
//! Cite: mesh-connectivity-daemon-design tip 6; crypto ≠ authorization.

use super::{authorize_probe, MeshUnderlayError, UnderlayAuthToken};
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[cfg(target_os = "linux")]
use super::linux::LinuxUnderlaySession;

const OPEN_REQ: &[u8] = b"bridge-tcp-open-v1";
const OPEN_ACK: &[u8] = b"bridge-tcp-open-ack-v1";
const OPEN_NACK: &[u8] = b"bridge-tcp-open-nack-v1";
const KIND_DATA: u8 = 1;
const KIND_CLOSE: u8 = 2;
/// Leave room for kind+len header inside tip-5 MAX_PLAINTEXT (4096).
const CHUNK_MAX: usize = 4_090;

fn encode_open_with_token(token: &str) -> Result<Vec<u8>, MeshUnderlayError> {
    let token_bytes = token.as_bytes();
    if token_bytes.len() > 4_096 {
        return Err(MeshUnderlayError::InvalidAuthToken);
    }
    let mut out = Vec::with_capacity(OPEN_REQ.len() + 2 + token_bytes.len());
    out.extend_from_slice(OPEN_REQ);
    out.extend_from_slice(&(token_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(token_bytes);
    Ok(out)
}

fn decode_open_with_token(packet: &[u8]) -> Result<&str, MeshUnderlayError> {
    if packet.len() < OPEN_REQ.len() + 2 || !packet.starts_with(OPEN_REQ) {
        return Err(MeshUnderlayError::Path(
            "expected bridge-tcp-open-v1 with token".into(),
        ));
    }
    let rest = &packet[OPEN_REQ.len()..];
    let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
    if rest.len() != 2 + len {
        return Err(MeshUnderlayError::Path(
            "bridge-tcp-open token length mismatch".into(),
        ));
    }
    std::str::from_utf8(&rest[2..])
        .map_err(|_| MeshUnderlayError::Path("bridge-tcp-open token not utf8".into()))
}

/// Client-side stream: HTTP+WS bytes in, framed underlay out (tip 6).
#[cfg(target_os = "linux")]
pub struct BridgeUnderlayClient {
    session: LinuxUnderlaySession,
    closed: bool,
}

#[cfg(target_os = "linux")]
impl BridgeUnderlayClient {
    /// Dial an already tip-5-handshaken session, send underlay token with OPEN,
    /// then carry application TCP bytes. Application `GATE4AGENT_BRIDGE_TOKEN`
    /// is still checked by the HTTP+WS door itself (second barrier).
    pub async fn open(
        mut session: LinuxUnderlaySession,
        provided_underlay_token: &str,
    ) -> Result<Self, MeshUnderlayError> {
        let open = encode_open_with_token(provided_underlay_token)?;
        session.send_encrypted(&open).await?;
        let ack = session.recv_encrypted().await?;
        if ack == OPEN_NACK {
            return Err(MeshUnderlayError::Unauthorized);
        }
        if ack != OPEN_ACK {
            return Err(MeshUnderlayError::Path(
                "bridge-tcp-open ack mismatch".into(),
            ));
        }
        Ok(Self {
            session,
            closed: false,
        })
    }

    pub async fn write_all(&mut self, mut data: &[u8]) -> Result<(), MeshUnderlayError> {
        if self.closed {
            return Err(MeshUnderlayError::Path(
                "bridge underlay client closed".into(),
            ));
        }
        while !data.is_empty() {
            let n = data.len().min(CHUNK_MAX);
            let mut frame = Vec::with_capacity(1 + 4 + n);
            frame.push(KIND_DATA);
            frame.extend_from_slice(&(n as u32).to_be_bytes());
            frame.extend_from_slice(&data[..n]);
            self.session.send_encrypted(&frame).await?;
            data = &data[n..];
        }
        Ok(())
    }

    pub async fn read_some(&mut self) -> Result<Vec<u8>, MeshUnderlayError> {
        if self.closed {
            return Ok(Vec::new());
        }
        let packet = self.session.recv_encrypted().await?;
        if packet.is_empty() {
            return Err(MeshUnderlayError::Path("empty bridge frame".into()));
        }
        match packet[0] {
            KIND_CLOSE => {
                self.closed = true;
                Ok(Vec::new())
            }
            KIND_DATA => {
                if packet.len() < 5 {
                    return Err(MeshUnderlayError::Path("short data frame".into()));
                }
                let len = u32::from_be_bytes(packet[1..5].try_into().unwrap()) as usize;
                if packet.len() != 5 + len {
                    return Err(MeshUnderlayError::Path(
                        "data frame length mismatch".into(),
                    ));
                }
                Ok(packet[5..].to_vec())
            }
            _ => Err(MeshUnderlayError::Path("unknown bridge frame kind".into())),
        }
    }

    /// Read until `min_bytes` accumulated or peer close (HTTP one-shots).
    pub async fn read_at_least(&mut self, min_bytes: usize) -> Result<Vec<u8>, MeshUnderlayError> {
        let mut out = Vec::new();
        while out.len() < min_bytes {
            let chunk = self.read_some().await?;
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    pub async fn close(mut self) -> Result<(), MeshUnderlayError> {
        if !self.closed {
            self.session.send_encrypted(&[KIND_CLOSE]).await?;
            self.closed = true;
        }
        Ok(())
    }
}

/// Server side: wait for OPEN+token, authorize underlay token, dial local
/// `--bridge-listen`, relay until close. HQ must never call this (accept
/// refused upstream). Application BRIDGE_TOKEN remains a separate barrier.
#[cfg(target_os = "linux")]
pub async fn serve_bridge_tcp_relay(
    mut session: LinuxUnderlaySession,
    underlay_auth: &UnderlayAuthToken,
    local_bridge: SocketAddr,
) -> Result<(), MeshUnderlayError> {
    let open = session.recv_encrypted().await?;
    let provided = decode_open_with_token(&open)?;
    if let Err(err) = authorize_probe(underlay_auth, Some(provided)) {
        let _ = session.send_encrypted(OPEN_NACK).await;
        return Err(err);
    }

    let mut tcp = TcpStream::connect(local_bridge)
        .await
        .map_err(|e| MeshUnderlayError::Path(format!("local bridge dial: {e}")))?;
    session.send_encrypted(OPEN_ACK).await?;

    let mut tcp_buf = vec![0u8; CHUNK_MAX];
    loop {
        tokio::select! {
            biased;
            read = tcp.read(&mut tcp_buf) => {
                match read {
                    Ok(0) => {
                        let _ = session.send_encrypted(&[KIND_CLOSE]).await;
                        break;
                    }
                    Ok(n) => {
                        let mut frame = Vec::with_capacity(1 + 4 + n);
                        frame.push(KIND_DATA);
                        frame.extend_from_slice(&(n as u32).to_be_bytes());
                        frame.extend_from_slice(&tcp_buf[..n]);
                        session.send_encrypted(&frame).await?;
                    }
                    Err(err) => {
                        let _ = session.send_encrypted(&[KIND_CLOSE]).await;
                        return Err(MeshUnderlayError::Path(format!("tcp read: {err}")));
                    }
                }
            }
            packet = session.recv_encrypted() => {
                let packet = packet?;
                if packet.is_empty() {
                    return Err(MeshUnderlayError::Path("empty bridge frame".into()));
                }
                match packet[0] {
                    KIND_CLOSE => {
                        let _ = tcp.shutdown().await;
                        break;
                    }
                    KIND_DATA => {
                        if packet.len() < 5 {
                            return Err(MeshUnderlayError::Path("short data frame".into()));
                        }
                        let len = u32::from_be_bytes(packet[1..5].try_into().unwrap()) as usize;
                        if packet.len() != 5 + len {
                            return Err(MeshUnderlayError::Path(
                                "data frame length mismatch".into(),
                            ));
                        }
                        tcp.write_all(&packet[5..])
                            .await
                            .map_err(|e| MeshUnderlayError::Path(format!("tcp write: {e}")))?;
                    }
                    _ => {
                        return Err(MeshUnderlayError::Path(
                            "unknown bridge frame kind".into(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Non-Linux refuse (Win/mac deferred).
#[cfg(not(target_os = "linux"))]
pub async fn serve_bridge_tcp_relay(
    _session: super::UnsupportedUnderlayHandle,
    _underlay_auth: &UnderlayAuthToken,
    _local_bridge: SocketAddr,
) -> Result<(), MeshUnderlayError> {
    Err(MeshUnderlayError::PlatformUnsupported {
        os: std::env::consts::OS,
        hint: "Linux-first tip 6 bridge-over-underlay; Win/mac later",
    })
}

#[cfg(not(target_os = "linux"))]
pub struct BridgeUnderlayClient;

/// Map underlay path errors that look like I/O into std::io::Error for node loops.
pub fn underlay_to_io(err: MeshUnderlayError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_encode_decode_round_trip() {
        let packet = encode_open_with_token("underlay-tip6-token").unwrap();
        assert!(packet.starts_with(OPEN_REQ));
        assert_eq!(decode_open_with_token(&packet).unwrap(), "underlay-tip6-token");
    }

    #[test]
    fn open_decode_rejects_truncated_and_bad_length() {
        assert!(decode_open_with_token(b"nope").is_err());
        assert!(decode_open_with_token(OPEN_REQ).is_err());
        let mut packet = OPEN_REQ.to_vec();
        packet.extend_from_slice(&5u16.to_be_bytes());
        packet.extend_from_slice(b"abcd"); // claims 5, has 4
        assert!(decode_open_with_token(&packet).is_err());
    }

    #[test]
    fn open_encode_rejects_oversized_token() {
        let huge = "x".repeat(5_000);
        assert_eq!(
            encode_open_with_token(&huge),
            Err(MeshUnderlayError::InvalidAuthToken)
        );
    }

    #[test]
    fn underlay_to_io_preserves_message_without_secrets() {
        let err = underlay_to_io(MeshUnderlayError::Unauthorized);
        let msg = err.to_string();
        assert!(msg.contains("unauthorized") || msg.contains("Unauthorized") || msg.contains("token"));
        assert!(!msg.contains("super-secret"));
    }
}
