//! Daemon liveness probe configuration and spec types.
//!
//! Used by DaemonHarness-class transports (e.g. OpenClaw) to verify the
//! daemon is alive before spawning a client process.

use std::time::Duration;

/// Probe configuration for checking daemon liveness before spawning a client.
///
/// A raw TCP connection is attempted to `host:port` within `timeout_ms`
/// milliseconds. If the handshake succeeds, the daemon is considered running.
#[derive(Debug, Clone)]
pub struct DaemonProbe {
    /// Hostname or IP address of the daemon gateway.
    pub host: String,
    /// TCP port the daemon is listening on.
    pub port: u16,
    /// Probe timeout in milliseconds. If the connection is not established
    /// within this window, `DaemonProbeTimeout` is returned.
    pub timeout_ms: u64,
}

impl DaemonProbe {
    /// Create a new `DaemonProbe`.
    pub fn new(host: impl Into<String>, port: u16, timeout_ms: u64) -> Self {
        Self {
            host: host.into(),
            port,
            timeout_ms,
        }
    }

    /// Return the probe timeout as a `Duration`.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// Full daemon specification: liveness probe + client-side spawn requirements.
///
/// Carried by DaemonHarness-class builders so the transport layer knows how
/// to verify the daemon and what error message to surface on failure.
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    /// Short name of the daemon (e.g. `"openclaw"`). Used in log messages.
    pub name: &'static str,
    /// Probe configuration used to check daemon liveness.
    pub probe: DaemonProbe,
    /// Human-readable install hint shown when the daemon is not running.
    /// Example: `"npm install -g acpx"`.
    pub install_hint: &'static str,
}
