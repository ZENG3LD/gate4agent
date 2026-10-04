//! HQ WireGuard client: one peer (C2), listen port 0, kernel interface.
//!
//! HQ is not a mesh peer. This module does not dial a node, does not install
//! a route to any address except the single C2 tunnel address, and does not
//! call the UDP+ChaCha mesh underlay or `open_wireguard_daemon_stub`.
//! Failure to create `ip link add type wireguard` is an error.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// One allowed IP. HQ accepts only the host route of the C2 tunnel address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HqWgAllowedIp {
    pub address: IpAddr,
    pub prefix_len: u8,
}

impl HqWgAllowedIp {
    pub fn host(address: IpAddr) -> Self {
        Self {
            address,
            prefix_len: match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            },
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (addr, prefix) = text.split_once('/')?;
        let address: IpAddr = addr.parse().ok()?;
        let prefix_len: u8 = prefix.parse().ok()?;
        let max = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix_len > max {
            return None;
        }
        Some(Self { address, prefix_len })
    }

    fn cidr(&self) -> String {
        format!("{}/{}", self.address, self.prefix_len)
    }
}

/// The single C2 peer HQ dials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HqWgPeer {
    pub public_key: String,
    /// C2's public WireGuard endpoint (`host:port`). `None` is rejected.
    pub endpoint: Option<SocketAddr>,
    pub allowed_ips: Vec<HqWgAllowedIp>,
    pub persistent_keepalive_secs: u16,
}

/// HQ-side client configuration. Listen port must be 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HqWgClientConfig {
    pub interface_name: String,
    pub private_key_path: PathBuf,
    pub listen_port: u16,
    pub hq_tunnel_address: IpAddr,
    pub c2_tunnel_address: IpAddr,
    pub control_port: u16,
    pub peers: Vec<HqWgPeer>,
}

impl HqWgClientConfig {
    pub fn validate(&self) -> Result<(), HqWgClientError> {
        if !valid_iface(&self.interface_name) {
            return Err(HqWgClientError::InterfaceName);
        }
        if self.private_key_path.as_os_str().is_empty() {
            return Err(HqWgClientError::PrivateKeyPath);
        }
        if self.listen_port != 0 {
            return Err(HqWgClientError::ListenPort(self.listen_port));
        }
        if self.control_port == 0 {
            return Err(HqWgClientError::ControlPort);
        }
        if self.hq_tunnel_address.is_unspecified()
            || self.c2_tunnel_address.is_unspecified()
            || self.hq_tunnel_address == self.c2_tunnel_address
            || std::mem::discriminant(&self.hq_tunnel_address)
                != std::mem::discriminant(&self.c2_tunnel_address)
        {
            return Err(HqWgClientError::TunnelAddress);
        }
        if self.peers.len() != 1 {
            return Err(HqWgClientError::PeerCount(self.peers.len()));
        }
        let peer = &self.peers[0];
        if peer.public_key.is_empty() {
            return Err(HqWgClientError::PeerKey);
        }
        match peer.endpoint {
            Some(endpoint) if !endpoint.ip().is_unspecified() && endpoint.port() != 0 => {}
            _ => return Err(HqWgClientError::MissingEndpoint),
        }
        if peer.persistent_keepalive_secs == 0 {
            return Err(HqWgClientError::KeepaliveDisabled);
        }
        if !allowed_ips_are_only_c2(&peer.allowed_ips, self.c2_tunnel_address) {
            return Err(HqWgClientError::AllowedIps);
        }
        Ok(())
    }

    /// `c2_tunnel_ip:control_port`. Never the public WireGuard endpoint.
    pub fn control_addr(&self) -> Result<SocketAddr, HqWgClientError> {
        self.validate()?;
        Ok(SocketAddr::new(self.c2_tunnel_address, self.control_port))
    }
}

fn allowed_ips_are_only_c2(allowed: &[HqWgAllowedIp], c2: IpAddr) -> bool {
    let Some(one) = allowed.first() else {
        return false;
    };
    if allowed.len() != 1 || one.address != c2 {
        return false;
    }
    match c2 {
        IpAddr::V4(_) => one.prefix_len == 32,
        IpAddr::V6(_) => one.prefix_len == 128,
    }
}

fn valid_iface(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..16).contains(&bytes.len())
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

#[derive(Debug, Error)]
pub enum HqWgClientError {
    #[error("HQ WireGuard client is missing the C2 public endpoint")]
    MissingEndpoint,
    #[error("HQ WireGuard listen port must be 0, got {0}")]
    ListenPort(u16),
    #[error("HQ WireGuard allowed IPs must be only the single C2 tunnel address")]
    AllowedIps,
    #[error("HQ WireGuard client must have exactly one peer (C2), got {0}")]
    PeerCount(usize),
    #[error("HQ WireGuard persistent keepalive must be on")]
    KeepaliveDisabled,
    #[error("HQ WireGuard interface name is invalid")]
    InterfaceName,
    #[error("HQ WireGuard private key path is empty")]
    PrivateKeyPath,
    #[error("HQ WireGuard tunnel addresses are invalid")]
    TunnelAddress,
    #[error("HQ WireGuard control port must be nonzero")]
    ControlPort,
    #[error("HQ WireGuard peer public key is empty")]
    PeerKey,
    #[error("kernel wireguard interface was not created: {0}")]
    InterfaceCreate(String),
    #[error("kernel wireguard setup failed: {0}")]
    Setup(String),
}

/// Creates the HQ interface with kernel WireGuard and installs a route only
/// to the C2 tunnel address. A failed `ip link add type wireguard` is returned
/// as [`HqWgClientError::InterfaceCreate`] with no other transport substituted.
pub fn bring_up_hq_wireguard(config: &HqWgClientConfig) -> Result<SocketAddr, HqWgClientError> {
    let control = config.control_addr()?;
    let peer = &config.peers[0];
    let endpoint = peer.endpoint.expect("validate requires an endpoint");
    let key_path = config.private_key_path.as_path();
    require_key_file(key_path)?;

    create_wireguard_iface(&config.interface_name)?;
    let configured = configure_wireguard(config, peer, endpoint);
    if let Err(error) = configured {
        delete_iface(&config.interface_name);
        return Err(error);
    }
    Ok(control)
}

fn require_key_file(path: &Path) -> Result<(), HqWgClientError> {
    let meta = std::fs::metadata(path).map_err(|error| {
        HqWgClientError::Setup(format!("private key path {}: {error}", path.display()))
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(HqWgClientError::Setup(format!(
            "private key path {} is not a nonempty file",
            path.display()
        )));
    }
    Ok(())
}

fn create_wireguard_iface(name: &str) -> Result<(), HqWgClientError> {
    let output = Command::new("ip")
        .args(["link", "add", "dev", name, "type", "wireguard"])
        .output()
        .map_err(|error| {
            HqWgClientError::InterfaceCreate(format!("ip link add type wireguard: {error}"))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(HqWgClientError::InterfaceCreate(format!(
        "ip link add dev {name} type wireguard failed: {stderr}"
    )))
}

fn configure_wireguard(
    config: &HqWgClientConfig,
    peer: &HqWgPeer,
    endpoint: SocketAddr,
) -> Result<(), HqWgClientError> {
    let name = &config.interface_name;
    let allowed = peer.allowed_ips[0].cidr();
    command_ok(
        "wg",
        &[
            "set",
            name,
            "listen-port",
            "0",
            "private-key",
            &config.private_key_path.display().to_string(),
            "peer",
            &peer.public_key,
            "endpoint",
            &endpoint.to_string(),
            "allowed-ips",
            &allowed,
            "persistent-keepalive",
            &peer.persistent_keepalive_secs.to_string(),
        ],
    )?;
    let prefix = match config.hq_tunnel_address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    let local = format!("{}/{}", config.hq_tunnel_address, prefix);
    command_ok("ip", &["addr", "replace", &local, "dev", name])?;
    command_ok("ip", &["link", "set", "up", "dev", name])?;
    let route = format!("{}/{}", config.c2_tunnel_address, if prefix == 32 { 32 } else { 128 });
    // AllowedIPs did not install a fib route on this kernel; the explicit
    // route is still only the C2 tunnel host, never a node prefix.
    command_ok("ip", &["route", "replace", &route, "dev", name])?;
    Ok(())
}

fn command_ok(program: &str, args: &[&str]) -> Result<(), HqWgClientError> {
    let output = Command::new(program).args(args).output().map_err(|error| {
        HqWgClientError::Setup(format!("{program}: {error}"))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(HqWgClientError::Setup(format!(
        "{program} {} failed: {stderr}",
        args.join(" ")
    )))
}

fn delete_iface(name: &str) {
    let _ = Command::new("ip").args(["link", "del", "dev", name]).status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HqWgClientConfig {
        HqWgClientConfig {
            interface_name: "wg-hq".to_owned(),
            private_key_path: PathBuf::from("/tmp/hq.priv"),
            listen_port: 0,
            hq_tunnel_address: "10.88.1.3".parse().unwrap(),
            c2_tunnel_address: "10.88.1.2".parse().unwrap(),
            control_port: 27441,
            peers: vec![HqWgPeer {
                public_key: "pyjbO05QwAyerdhvLEogM9JRv8P8Im52v2b3P1KHfwM=".to_owned(),
                endpoint: Some("192.168.78.2:51820".parse().unwrap()),
                allowed_ips: vec![HqWgAllowedIp::host("10.88.1.2".parse().unwrap())],
                persistent_keepalive_secs: 25,
            }],
        }
    }

    #[test]
    fn single_c2_tunnel_host_is_accepted() {
        let config = sample();
        assert!(config.validate().is_ok());
        assert_eq!(
            config.control_addr().unwrap(),
            "10.88.1.2:27441".parse().unwrap()
        );
    }

    #[test]
    fn missing_endpoint_is_rejected() {
        let mut config = sample();
        config.peers[0].endpoint = None;
        assert!(matches!(config.validate(), Err(HqWgClientError::MissingEndpoint)));
        config.peers[0].endpoint = Some("0.0.0.0:51820".parse().unwrap());
        assert!(matches!(config.validate(), Err(HqWgClientError::MissingEndpoint)));
    }

    #[test]
    fn listen_port_other_than_zero_is_rejected() {
        let mut config = sample();
        config.listen_port = 51820;
        assert!(matches!(config.validate(), Err(HqWgClientError::ListenPort(51820))));
    }

    #[test]
    fn allowed_ips_other_than_the_c2_tunnel_address_are_rejected() {
        let node: IpAddr = "10.88.1.1".parse().unwrap();
        let c2: IpAddr = "10.88.1.2".parse().unwrap();
        let cases = [
            vec![HqWgAllowedIp { address: "10.88.1.0".parse().unwrap(), prefix_len: 24 }],
            vec![HqWgAllowedIp { address: c2, prefix_len: 24 }],
            vec![HqWgAllowedIp::host(c2), HqWgAllowedIp::host(node)],
            vec![HqWgAllowedIp::host(node)],
            vec![],
        ];
        for allowed in cases {
            let mut config = sample();
            config.peers[0].allowed_ips = allowed;
            assert!(
                matches!(config.validate(), Err(HqWgClientError::AllowedIps)),
                "allowed-ips must be only 10.88.1.2/32"
            );
        }
    }
}
