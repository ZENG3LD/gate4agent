//! Synchronous TCP probe for daemon liveness.
//!
//! Uses `std::net` raw TCP only — no reqwest, no async, no HTTP. This is
//! intentional: the probe must be callable from synchronous contexts and must
//! not introduce an HTTP client dependency (PRD non-negotiable).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};

use crate::error::AgentError;
use crate::transport::DaemonProbe;

/// Attempt a raw TCP connection to the daemon within the probe timeout.
///
/// Returns `Ok(())` if the TCP handshake completes successfully, meaning the
/// daemon is reachable and accepting connections.
///
/// Returns `Err(AgentError::DaemonNotRunning)` if:
/// - The address cannot be resolved.
/// - The connection is refused (e.g. daemon is not running).
/// - Any other non-timeout I/O error occurs.
///
/// Returns `Err(AgentError::DaemonProbeTimeout)` if the connection attempt
/// does not complete within the probe timeout window.
///
/// # No HTTP dependency
/// This function uses `std::net::TcpStream::connect_timeout` only — no reqwest,
/// no HTTP, no async. The TCP handshake alone is sufficient to verify the daemon
/// socket is open.
pub fn probe_daemon(spec: &DaemonProbe) -> Result<(), AgentError> {
    let addr_str = format!("{}:{}", spec.host, spec.port);

    let addr: SocketAddr = match addr_str.to_socket_addrs() {
        Ok(mut iter) => match iter.next() {
            Some(a) => a,
            None => {
                return Err(AgentError::DaemonNotRunning {
                    host: spec.host.clone(),
                    port: spec.port,
                    detail: "no resolvable address".into(),
                });
            }
        },
        Err(e) => {
            return Err(AgentError::DaemonNotRunning {
                host: spec.host.clone(),
                port: spec.port,
                detail: format!("address resolution failed: {}", e),
            });
        }
    };

    match TcpStream::connect_timeout(&addr, spec.timeout()) {
        Ok(_stream) => Ok(()),
        Err(e)
            if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Err(AgentError::DaemonProbeTimeout {
                host: spec.host.clone(),
                port: spec.port,
                timeout_ms: spec.timeout_ms,
            })
        }
        Err(e) => Err(AgentError::DaemonNotRunning {
            host: spec.host.clone(),
            port: spec.port,
            detail: format!("connect failed: {}", e),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Bind an OS-assigned port, probe it — should succeed.
    #[test]
    fn probe_succeeds_on_open_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
        let port = listener.local_addr().expect("local_addr").port();

        let spec = DaemonProbe::new("127.0.0.1", port, 2000);
        let result = probe_daemon(&spec);

        drop(listener);

        assert!(
            result.is_ok(),
            "probe must succeed when a listener is bound: {:?}",
            result
        );
    }

    /// Bind a port to get a free number, immediately drop the listener, then probe.
    /// Should return DaemonNotRunning (or DaemonProbeTimeout on unusual Windows behaviour).
    ///
    /// On Windows, closed-socket probes sometimes return ConnectionRefused immediately
    /// (maps to DaemonNotRunning) and sometimes time out (maps to DaemonProbeTimeout).
    /// Both outcomes are acceptable — what matters is that an error is returned.
    #[test]
    fn probe_returns_not_running_on_closed_port() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
            listener.local_addr().expect("local_addr").port()
            // listener drops here, freeing the port
        };

        let spec = DaemonProbe::new("127.0.0.1", port, 500);
        let result = probe_daemon(&spec);

        assert!(
            matches!(
                result,
                Err(AgentError::DaemonNotRunning { .. })
                    | Err(AgentError::DaemonProbeTimeout { .. })
            ),
            "probe on closed port must return DaemonNotRunning or DaemonProbeTimeout, got: {:?}",
            result
        );
    }

    /// Probe 192.0.2.1:1 (TEST-NET-1, RFC 5737 — guaranteed unroutable) with a
    /// short timeout to exercise the timeout path.
    ///
    /// Excluded on Windows because Windows can behave erratically with unroutable
    /// addresses (sometimes RST immediately, sometimes hangs past our timeout).
    /// On Linux/macOS the packet is dropped and the probe times out predictably.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn probe_returns_error_on_blackhole() {
        // 192.0.2.1 is TEST-NET-1 (RFC 5737) — reserved, unroutable, packets are dropped.
        // We expect either DaemonProbeTimeout (packet silently dropped) or
        // DaemonNotRunning (some networks send ICMP unreachable back immediately).
        let spec = DaemonProbe::new("192.0.2.1", 1, 200);
        let result = probe_daemon(&spec);

        assert!(
            matches!(
                result,
                Err(AgentError::DaemonProbeTimeout { .. })
                    | Err(AgentError::DaemonNotRunning { .. })
            ),
            "probe on blackhole address must return an error, got: {:?}",
            result
        );
    }
}
