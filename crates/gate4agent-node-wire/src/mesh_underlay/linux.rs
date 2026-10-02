//! Linux userspace underlay path: UDP + ChaCha20-Poly1305.
//!
//! Tip 5 deliberately does **not** open in-kernel WireGuard or require
//! CAP_NET_ADMIN. TUN surface is probed for operator honesty only.

use super::{
    assert_accept_allowed, authorize_probe, MeshUnderlayError, MeshUnderlayRole, UnderlayAuthToken,
    UnderlayTransportKey,
};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::UdpSocket;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAX_PLAINTEXT: usize = 4_096;

/// Listener side (C2 or node accept-peer).
pub struct LinuxUnderlayListener {
    socket: UdpSocket,
    key: LessSafeKey,
    auth: UnderlayAuthToken,
    /// Monotonic send counter for nonce uniqueness on accepted sessions.
    send_counter: u64,
}

/// Established encrypted underlay session (either dial or accept side).
pub struct LinuxUnderlaySession {
    socket: UdpSocket,
    #[allow(dead_code)]
    peer: SocketAddr,
    key: LessSafeKey,
    auth: UnderlayAuthToken,
    send_counter: u64,
}

/// Informational TUN/CAP report — not required for the UDP+AEAD path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunSurfaceReport {
    pub device_present: bool,
    pub open_attempt: &'static str,
    pub note: &'static str,
}

pub fn probe_tun_surface() -> TunSurfaceReport {
    let device_present = Path::new("/dev/net/tun").exists();
    if !device_present {
        return TunSurfaceReport {
            device_present: false,
            open_attempt: "missing-device",
            note: "tip 5 uses userspace UDP+AEAD; kernel TUN/WG deferred",
        };
    }
    // Best-effort open; do not claim a named iface (needs CAP_NET_ADMIN + ioctl).
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
    {
        Ok(_f) => TunSurfaceReport {
            device_present: true,
            open_attempt: "ok",
            note: "tip 5 does not configure WireGuard; CAP_NET_ADMIN may still be needed for iface",
        },
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => TunSurfaceReport {
            device_present: true,
            open_attempt: "permission-denied",
            note: "CAP_NET_ADMIN / device cgroup likely required for TUN; tip 5 UDP path does not need it",
        },
        Err(_) => TunSurfaceReport {
            device_present: true,
            open_attempt: "error",
            note: "TUN present but open failed; tip 5 continues on userspace UDP+AEAD",
        },
    }
}

fn sealing_key(transport: &UnderlayTransportKey) -> Result<LessSafeKey, MeshUnderlayError> {
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, transport.as_bytes())
        .map_err(|_| MeshUnderlayError::InvalidTransportKey)?;
    Ok(LessSafeKey::new(unbound))
}


pub async fn accept_peer(
    role: MeshUnderlayRole,
    transport_key: &UnderlayTransportKey,
    auth_token: &UnderlayAuthToken,
) -> Result<LinuxUnderlayListener, MeshUnderlayError> {
    assert_accept_allowed(role)?;
    let key = sealing_key(transport_key)?;
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;
    Ok(LinuxUnderlayListener {
        socket,
        key,
        auth: auth_token.clone(),
        send_counter: 0,
    })
}

pub async fn dial_peer(
    role: MeshUnderlayRole,
    transport_key: &UnderlayTransportKey,
    auth_token: &UnderlayAuthToken,
    peer: &str,
) -> Result<LinuxUnderlaySession, MeshUnderlayError> {
    // Dial is allowed for all roles including HQ (HQ always dials).
    let _ = role;
    let key = sealing_key(transport_key)?;
    let peer: SocketAddr = peer
        .parse()
        .map_err(|e: std::net::AddrParseError| MeshUnderlayError::Path(e.to_string()))?;
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;
    socket
        .connect(peer)
        .await
        .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;

    // Handshake: dialer sends encrypted "mesh-underlay-v1-hello".
    let mut session = LinuxUnderlaySession {
        socket,
        peer,
        key,
        auth: auth_token.clone(),
        send_counter: 0,
    };
    session.send_encrypted(b"mesh-underlay-v1-hello").await?;
    let reply = session.recv_encrypted().await?;
    if reply != b"mesh-underlay-v1-hello-ack" {
        return Err(MeshUnderlayError::Path(
            "underlay hello ack mismatch".into(),
        ));
    }
    Ok(session)
}

impl LinuxUnderlayListener {
    pub fn local_addr(&self) -> Result<SocketAddr, MeshUnderlayError> {
        self.socket
            .local_addr()
            .map_err(|e| MeshUnderlayError::Path(e.to_string()))
    }

    pub async fn accept(self) -> Result<LinuxUnderlaySession, MeshUnderlayError> {
        // First datagram completes handshake from dialer.
        let mut buf = vec![0u8; NONCE_LEN + MAX_PLAINTEXT + TAG_LEN];
        let (n, peer) = self
            .socket
            .recv_from(&mut buf)
            .await
            .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;
        buf.truncate(n);

        let mut session = LinuxUnderlaySession {
            socket: self.socket,
            peer,
            key: self.key,
            auth: self.auth,
            send_counter: self.send_counter,
        };
        // Connect so subsequent send/recv are peer-scoped.
        session
            .socket
            .connect(peer)
            .await
            .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;

        let plain = decrypt(&session.key, &buf)?;
        if plain != b"mesh-underlay-v1-hello" {
            return Err(MeshUnderlayError::Path(
                "underlay hello mismatch".into(),
            ));
        }
        session
            .send_encrypted(b"mesh-underlay-v1-hello-ack")
            .await?;
        Ok(session)
    }
}

impl LinuxUnderlaySession {
    pub async fn send_encrypted(&mut self, plaintext: &[u8]) -> Result<(), MeshUnderlayError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(MeshUnderlayError::Path("plaintext too large".into()));
        }
        let counter = self.send_counter;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or_else(|| MeshUnderlayError::Path("nonce counter overflow".into()))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes[4..].copy_from_slice(&counter.to_be_bytes());
        let mut out = Vec::with_capacity(NONCE_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(&nonce_bytes);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut body = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut body)
            .map_err(|_| MeshUnderlayError::Path("seal failed".into()))?;
        out.extend_from_slice(&body);
        self.socket
            .send(&out)
            .await
            .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;
        Ok(())
    }

    pub async fn recv_encrypted(&self) -> Result<Vec<u8>, MeshUnderlayError> {
        let mut buf = vec![0u8; NONCE_LEN + MAX_PLAINTEXT + TAG_LEN];
        let n = self
            .socket
            .recv(&mut buf)
            .await
            .map_err(|e| MeshUnderlayError::Path(e.to_string()))?;
        buf.truncate(n);
        decrypt(&self.key, &buf)
    }

    /// Probe action: token barrier still required even when crypto session is live.
    pub fn probe(&self, provided_token: Option<&str>) -> Result<(), MeshUnderlayError> {
        authorize_probe(&self.auth, provided_token)
    }
}

fn decrypt(key: &LessSafeKey, packet: &[u8]) -> Result<Vec<u8>, MeshUnderlayError> {
    if packet.len() < NONCE_LEN + TAG_LEN {
        return Err(MeshUnderlayError::Path("datagram too short".into()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(nonce_bytes);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);
    let mut body = ct.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut body)
        .map_err(|_| MeshUnderlayError::Path("open failed".into()))?;
    Ok(plain.to_vec())
}
