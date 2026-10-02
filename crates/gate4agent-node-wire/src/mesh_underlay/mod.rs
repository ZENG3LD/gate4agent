//! Minimal Linux-first mesh **underlay** slice (design tips 5–6).
//!
//! Not a WireGuard daemon product. Provides:
//! - peer dial/accept role lock (C2+node DialOrAccept; HQ DialOnly)
//! - userspace UDP + AEAD path on Linux (encrypted pipe)
//! - **separate** authorization token barrier for probe actions
//!   (transport crypto ≠ authorization)
//! - clear refuse on Win/mac and for HQ underlay accept
//! - tip 6: TCP bridge-reach over underlay (HTTP+WS dialect unchanged)
//!
//! Cite:
//! - `mesh-connectivity-daemon-design-2026-10-02.md` §1.2 / §1.5 / tips 5–6
//! - `mesh-underlay-linux-tun-wg-vs-win-mac-2026-10-02.md`
//! - hatchery `mesh_role` DialOnly lock
//!
//! Feature: `mesh-underlay` (default on). Disable to omit this module from
//! dependents that do not need tip-5/6 surfaces.

use std::fmt;

#[cfg(target_os = "linux")]
mod linux;
mod bridge_reach;
#[cfg(target_os = "linux")]
pub use linux::{
    accept_peer, accept_peer_on, dial_peer, probe_tun_surface, LinuxUnderlayListener,
    LinuxUnderlaySession, TunSurfaceReport,
};
pub use bridge_reach::{serve_bridge_tcp_relay, underlay_to_io, BridgeUnderlayClient};

/// Who participates on the underlay (mirrors hatchery `MeshParticipantRole`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeshUnderlayRole {
    C2Peer,
    NodePeer,
    /// Hatchery HQ — always dial-only; never underlay accept.
    HqClientAdmin,
}

/// Dial capability derived from role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeshUnderlayDialCapability {
    DialOnly,
    DialOrAccept,
}

impl MeshUnderlayRole {
    pub const fn dial_capability(self) -> MeshUnderlayDialCapability {
        match self {
            Self::C2Peer | Self::NodePeer => MeshUnderlayDialCapability::DialOrAccept,
            Self::HqClientAdmin => MeshUnderlayDialCapability::DialOnly,
        }
    }

    pub const fn may_accept_underlay(self) -> bool {
        matches!(
            self.dial_capability(),
            MeshUnderlayDialCapability::DialOrAccept
        )
    }
}

impl fmt::Display for MeshUnderlayRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::C2Peer => "c2-peer",
            Self::NodePeer => "node-peer",
            Self::HqClientAdmin => "hq-client-admin",
        })
    }
}

/// Errors for the tip-5 underlay slice (never carry token material).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshUnderlayError {
    /// HQ / DialOnly must not accept underlay peers.
    HqMustNotAcceptUnderlay,
    /// Win/mac (and non-Linux) underlay not implemented this tip.
    PlatformUnsupported {
        os: &'static str,
        hint: &'static str,
    },
    /// Probe / action refused — token barrier failed (crypto session may still be up).
    Unauthorized,
    /// Transport key rejected (length / empty).
    InvalidTransportKey,
    /// Auth token rejected at configuration time (empty / oversized).
    InvalidAuthToken,
    /// I/O or framing failure on the underlay path.
    Path(String),
    /// Full WireGuard / kernel TUN datapath not opened this tip.
    WireGuardDaemonNotInThisTip,
}

impl fmt::Display for MeshUnderlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HqMustNotAcceptUnderlay => {
                f.write_str("HQ mesh role is dial-only; underlay accept is refused")
            }
            Self::PlatformUnsupported { os, hint } => {
                write!(f, "mesh underlay unsupported on {os}: {hint}")
            }
            Self::Unauthorized => {
                f.write_str("underlay probe unauthorized: token barrier failed (crypto ≠ auth)")
            }
            Self::InvalidTransportKey => {
                f.write_str("underlay transport key must be exactly 32 bytes")
            }
            Self::InvalidAuthToken => {
                f.write_str("underlay auth token empty or longer than 4096 bytes")
            }
            Self::Path(msg) => write!(f, "underlay path error: {msg}"),
            Self::WireGuardDaemonNotInThisTip => f.write_str(
                "no WireGuard/kernel TUN daemon in tips 5–6; userspace UDP+AEAD + bridge TCP relay only",
            ),
        }
    }
}

impl std::error::Error for MeshUnderlayError {}

/// 32-byte transport key for underlay AEAD (encrypts the pipe — **not** auth).
#[derive(Clone)]
pub struct UnderlayTransportKey {
    bytes: [u8; 32],
}

impl UnderlayTransportKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, MeshUnderlayError> {
        let Ok(array) = <[u8; 32]>::try_from(bytes) else {
            return Err(MeshUnderlayError::InvalidTransportKey);
        };
        Ok(Self { bytes: array })
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl fmt::Debug for UnderlayTransportKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnderlayTransportKey([redacted])")
    }
}

/// Authorization token barrier **inside** the encrypted underlay path.
/// Distinct from [`UnderlayTransportKey`]. Never logged.
#[derive(Clone)]
pub struct UnderlayAuthToken {
    value: String,
}

impl UnderlayAuthToken {
    pub fn new(value: impl Into<String>) -> Result<Self, MeshUnderlayError> {
        let value = value.into();
        if value.is_empty() || value.len() > 4_096 {
            return Err(MeshUnderlayError::InvalidAuthToken);
        }
        Ok(Self { value })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for UnderlayAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnderlayAuthToken([redacted])")
    }
}

/// Constant-time equality for token barriers (spirit of C2/bridge gates).
pub fn tokens_match(provided: &str, expected: &str) -> bool {
    let left = provided.as_bytes();
    let right = expected.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Check authorization for a probe action. Encrypted path being up is irrelevant.
pub fn authorize_probe(
    configured: &UnderlayAuthToken,
    provided: Option<&str>,
) -> Result<(), MeshUnderlayError> {
    match provided {
        Some(got) if tokens_match(got, configured.as_str()) => Ok(()),
        _ => Err(MeshUnderlayError::Unauthorized),
    }
}

/// Refuse underlay accept when role is dial-only (HQ).
pub fn assert_accept_allowed(role: MeshUnderlayRole) -> Result<(), MeshUnderlayError> {
    if role.may_accept_underlay() {
        Ok(())
    } else {
        Err(MeshUnderlayError::HqMustNotAcceptUnderlay)
    }
}

/// Non-Linux entry points: clear platform refuse (Win/mac stub).
#[cfg(not(target_os = "linux"))]
pub fn accept_peer(
    role: MeshUnderlayRole,
    _transport_key: &UnderlayTransportKey,
    _auth_token: &UnderlayAuthToken,
) -> Result<UnsupportedUnderlayHandle, MeshUnderlayError> {
    assert_accept_allowed(role)?;
    Err(platform_unsupported())
}

#[cfg(not(target_os = "linux"))]
pub fn accept_peer_on(
    role: MeshUnderlayRole,
    transport_key: &UnderlayTransportKey,
    auth_token: &UnderlayAuthToken,
    _bind: std::net::SocketAddr,
) -> Result<UnsupportedUnderlayHandle, MeshUnderlayError> {
    accept_peer(role, transport_key, auth_token)
}

#[cfg(not(target_os = "linux"))]
pub fn dial_peer(
    _role: MeshUnderlayRole,
    _transport_key: &UnderlayTransportKey,
    _auth_token: &UnderlayAuthToken,
    _peer: &str,
) -> Result<UnsupportedUnderlayHandle, MeshUnderlayError> {
    Err(platform_unsupported())
}

#[cfg(not(target_os = "linux"))]
pub fn probe_tun_surface() -> TunSurfaceReport {
    TunSurfaceReport {
        device_present: false,
        open_attempt: "skipped-non-linux",
        note: "Win/mac underlay deferred; see recon mesh-underlay-linux-tun-wg-vs-win-mac",
    }
}

#[cfg(not(target_os = "linux"))]
fn platform_unsupported() -> MeshUnderlayError {
    MeshUnderlayError::PlatformUnsupported {
        os: std::env::consts::OS,
        hint: "Linux-first tip 5; Win/mac underlay later (WireGuardNT/utun) — see recon doc",
    }
}

/// Placeholder handle type on non-Linux so signatures stay symmetrical.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct UnsupportedUnderlayHandle;

/// TUN surface report (informational; tip 5 does not require CAP_NET_ADMIN).
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunSurfaceReport {
    pub device_present: bool,
    pub open_attempt: &'static str,
    pub note: &'static str,
}

/// Explicit refuse of full WG daemon product this tip.
pub fn open_wireguard_daemon_stub() -> Result<(), MeshUnderlayError> {
    Err(MeshUnderlayError::WireGuardDaemonNotInThisTip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hq_is_dial_only_accept_refused() {
        assert!(!MeshUnderlayRole::HqClientAdmin.may_accept_underlay());
        assert_eq!(
            assert_accept_allowed(MeshUnderlayRole::HqClientAdmin),
            Err(MeshUnderlayError::HqMustNotAcceptUnderlay)
        );
        for role in [MeshUnderlayRole::C2Peer, MeshUnderlayRole::NodePeer] {
            assert!(role.may_accept_underlay());
            assert_eq!(assert_accept_allowed(role), Ok(()));
        }
    }

    #[test]
    fn token_barrier_independent_of_transport_key() {
        let token = UnderlayAuthToken::new("probe-secret").unwrap();
        assert!(authorize_probe(&token, Some("probe-secret")).is_ok());
        assert_eq!(
            authorize_probe(&token, Some("wrong-secret!!")),
            Err(MeshUnderlayError::Unauthorized)
        );
        assert_eq!(
            authorize_probe(&token, None),
            Err(MeshUnderlayError::Unauthorized)
        );
        // Transport key presence is orthogonal — constructing one does not authorize.
        let _key = UnderlayTransportKey::from_bytes([7u8; 32]);
        assert_eq!(
            authorize_probe(&token, Some("still-wrong")),
            Err(MeshUnderlayError::Unauthorized)
        );
    }

    #[test]
    fn wireguard_daemon_stub_refuses() {
        assert_eq!(
            open_wireguard_daemon_stub(),
            Err(MeshUnderlayError::WireGuardDaemonNotInThisTip)
        );
    }

    #[test]
    fn secrets_redacted_in_debug() {
        let key = UnderlayTransportKey::from_bytes([1u8; 32]);
        let token = UnderlayAuthToken::new("super-secret-token").unwrap();
        let key_dbg = format!("{key:?}");
        let token_dbg = format!("{token:?}");
        assert!(!key_dbg.contains("1, 1, 1"));
        assert!(!token_dbg.contains("super-secret"));
        assert!(key_dbg.contains("redacted"));
        assert!(token_dbg.contains("redacted"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_peer_encrypted_path_still_requires_token_for_probe() {
        let transport = UnderlayTransportKey::from_bytes([9u8; 32]);
        let auth = UnderlayAuthToken::new("underlay-probe-token").unwrap();

        // HQ must not accept even on Linux.
        assert_eq!(
            accept_peer(MeshUnderlayRole::HqClientAdmin, &transport, &auth)
                .await
                .err(),
            Some(MeshUnderlayError::HqMustNotAcceptUnderlay)
        );

        let listener = accept_peer(MeshUnderlayRole::C2Peer, &transport, &auth)
            .await
            .expect("c2 peer may accept");
        let addr = listener.local_addr().expect("bound");

        let dial_task = {
            let transport = transport.clone();
            let auth = auth.clone();
            tokio::spawn(async move {
                dial_peer(
                    MeshUnderlayRole::NodePeer,
                    &transport,
                    &auth,
                    &addr.to_string(),
                )
                .await
            })
        };

        let server = listener
            .accept()
            .await
            .expect("accept encrypted peer session");
        let mut client = dial_task.await.expect("join").expect("dial ok");

        // Encrypted round-trip hello (crypto up).
        client
            .send_encrypted(b"hello-from-node")
            .await
            .expect("client send");
        let got = server.recv_encrypted().await.expect("server recv");
        assert_eq!(got, b"hello-from-node");

        // Probe without / wrong token fails even though crypto session is live.
        assert_eq!(
            client.probe(None),
            Err(MeshUnderlayError::Unauthorized)
        );
        assert_eq!(
            client.probe(Some("wrong")),
            Err(MeshUnderlayError::Unauthorized)
        );
        client
            .probe(Some("underlay-probe-token"))
            .expect("probe authorized");
        server
            .probe(Some("underlay-probe-token"))
            .expect("server probe authorized");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_tun_surface_report_is_informative() {
        let report = probe_tun_surface();
        // /dev/net/tun often exists in containers; open may still need CAP_NET_ADMIN.
        assert!(
            report.open_attempt == "ok"
                || report.open_attempt == "permission-denied"
                || report.open_attempt == "error"
                || report.open_attempt == "missing-device"
        );
        assert!(report.note.contains("tip 5") || report.note.contains("CAP_NET_ADMIN") || report.note.contains("userspace"));
    }


    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn tip6_bridge_health_over_underlay_requires_token_barriers() {
        use crate::mesh_underlay::{serve_bridge_tcp_relay, BridgeUnderlayClient};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let transport = UnderlayTransportKey::from_bytes([11u8; 32]);
        let underlay_auth = UnderlayAuthToken::new("underlay-tip6-token").unwrap();

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_addr = tcp.local_addr().unwrap();
        let app_task = tokio::spawn(async move {
            let (mut stream, _) = tcp.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = br#"{"ok":true,"door":"node-envelope-bridge","tip":6}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).unwrap()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });

        assert_eq!(
            accept_peer(MeshUnderlayRole::HqClientAdmin, &transport, &underlay_auth)
                .await
                .err(),
            Some(MeshUnderlayError::HqMustNotAcceptUnderlay)
        );

        // --- unauthorized path ---
        let listener = accept_peer(MeshUnderlayRole::NodePeer, &transport, &underlay_auth)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let client_bad = {
            let transport = transport.clone();
            let underlay_auth = underlay_auth.clone();
            tokio::spawn(async move {
                let session = dial_peer(
                    MeshUnderlayRole::C2Peer,
                    &transport,
                    &underlay_auth,
                    &addr.to_string(),
                )
                .await
                .unwrap();
                BridgeUnderlayClient::open(session, "wrong-token").await
            })
        };
        let session = listener.accept().await.unwrap();
        let server_err = serve_bridge_tcp_relay(session, &underlay_auth, bridge_addr).await;
        assert!(matches!(server_err, Err(MeshUnderlayError::Unauthorized)));
        let client_err = client_bad.await.unwrap();
        assert!(matches!(client_err, Err(MeshUnderlayError::Unauthorized)));

        // --- authorized path ---
        let listener = accept_peer(MeshUnderlayRole::NodePeer, &transport, &underlay_auth)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let client_ok = {
            let transport = transport.clone();
            let underlay_auth = underlay_auth.clone();
            tokio::spawn(async move {
                let session = dial_peer(
                    MeshUnderlayRole::C2Peer,
                    &transport,
                    &underlay_auth,
                    &addr.to_string(),
                )
                .await
                .unwrap();
                let mut client = BridgeUnderlayClient::open(session, "underlay-tip6-token")
                    .await
                    .unwrap();
                client
                    .write_all(b"GET /bridge/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
                let resp = client.read_at_least(12).await.unwrap();
                let text = String::from_utf8_lossy(&resp);
                assert!(text.contains("200 OK"), "got {text}");
                assert!(text.contains("node-envelope-bridge"));
                client.close().await.unwrap();
            })
        };
        let session = listener.accept().await.unwrap();
        serve_bridge_tcp_relay(session, &underlay_auth, bridge_addr)
            .await
            .expect("authorized relay");
        client_ok.await.unwrap();
        app_task.await.unwrap();
    }


    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn tip6_bridge_multi_chunk_relay_over_underlay() {
        use crate::mesh_underlay::{serve_bridge_tcp_relay, BridgeUnderlayClient};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let transport = UnderlayTransportKey::from_bytes([13u8; 32]);
        let underlay_auth = UnderlayAuthToken::new("underlay-tip6-chunk").unwrap();

        // Local app door echoes a large body so client must reassemble chunks.
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_addr = tcp.local_addr().unwrap();
        let payload = vec![0x5Au8; 9_000]; // > CHUNK_MAX (4090) — forces multi-frame
        let app_task = {
            let payload = payload.clone();
            tokio::spawn(async move {
                let (mut stream, _) = tcp.accept().await.unwrap();
                let mut buf = vec![0u8; 64];
                let _ = stream.read(&mut buf).await;
                stream.write_all(&payload).await.unwrap();
                let _ = stream.shutdown().await;
            })
        };

        let listener = accept_peer(MeshUnderlayRole::NodePeer, &transport, &underlay_auth)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let client_task = {
            let transport = transport.clone();
            let underlay_auth = underlay_auth.clone();
            let expect = payload.clone();
            tokio::spawn(async move {
                let session = dial_peer(
                    MeshUnderlayRole::C2Peer,
                    &transport,
                    &underlay_auth,
                    &addr.to_string(),
                )
                .await
                .unwrap();
                let mut client = BridgeUnderlayClient::open(session, "underlay-tip6-chunk")
                    .await
                    .unwrap();
                client.write_all(b"GET /echo HTTP/1.1\r\n\r\n").await.unwrap();
                let got = client.read_at_least(expect.len()).await.unwrap();
                assert_eq!(got, expect, "multi-chunk reassembly mismatch");
                client.close().await.unwrap();
            })
        };
        let session = listener.accept().await.unwrap();
        serve_bridge_tcp_relay(session, &underlay_auth, bridge_addr)
            .await
            .expect("authorized multi-chunk relay");
        client_task.await.unwrap();
        app_task.await.unwrap();
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_accept_refuses_with_clear_platform_error() {
        let transport = UnderlayTransportKey::from_bytes([3u8; 32]);
        let auth = UnderlayAuthToken::new("t").unwrap();
        let err = accept_peer(MeshUnderlayRole::C2Peer, &transport, &auth).unwrap_err();
        match err {
            MeshUnderlayError::PlatformUnsupported { hint, .. } => {
                assert!(hint.contains("Linux-first"));
            }
            other => panic!("expected PlatformUnsupported, got {other:?}"),
        }
    }
}
