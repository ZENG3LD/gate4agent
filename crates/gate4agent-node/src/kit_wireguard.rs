//! Kernel WireGuard peer for node↔C2.
//!
//! Node and C2 are peers: either side may dial. This is `ip link add type
//! wireguard` plus `wg set`. It does not open the UDP+ChaCha mesh underlay
//! and it does not call the mesh daemon stub. HQ is not configured
//! here. The driver may be replaced later; this build uses kernel WireGuard.
//! A failed `ip link add type wireguard` is an error, not a fallback.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// Link tag so the kit binary retains this module.
pub const LINK_TAG: &str = "kernel-wireguard-node-c2-peer";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeWgPeerConfig {
    pub interface_name: String,
    pub private_key_path: PathBuf,
    /// `0` means this side does not accept. Then [`Self::peer_endpoint`] is required.
    pub listen_port: u16,
    pub local_tunnel_address: IpAddr,
    pub peer_tunnel_address: IpAddr,
    pub peer_public_key: String,
    /// C2's public endpoint. `None` means C2 dials us, so [`Self::listen_port`] must be nonzero.
    pub peer_endpoint: Option<SocketAddr>,
    pub persistent_keepalive_secs: u16,
}

impl NodeWgPeerConfig {
    pub fn validate(&self) -> Result<(), NodeWgPeerError> {
        if !valid_iface(&self.interface_name) {
            return Err(NodeWgPeerError::InterfaceName);
        }
        if self.private_key_path.as_os_str().is_empty() {
            return Err(NodeWgPeerError::PrivateKeyPath);
        }
        if self.peer_public_key.is_empty() {
            return Err(NodeWgPeerError::PeerKey);
        }
        if self.local_tunnel_address.is_unspecified()
            || self.peer_tunnel_address.is_unspecified()
            || self.local_tunnel_address == self.peer_tunnel_address
            || std::mem::discriminant(&self.local_tunnel_address)
                != std::mem::discriminant(&self.peer_tunnel_address)
        {
            return Err(NodeWgPeerError::TunnelAddress);
        }
        match self.peer_endpoint {
            Some(endpoint) if endpoint.ip().is_unspecified() || endpoint.port() == 0 => {
                return Err(NodeWgPeerError::Endpoint);
            }
            Some(_) => {}
            None if self.listen_port == 0 => return Err(NodeWgPeerError::NobodyDials),
            None => {}
        }
        Ok(())
    }

    /// TCP address of the C2 peer on the tunnel, for a later control dial.
    /// Not the public WireGuard endpoint.
    pub fn peer_tunnel_socket(&self, control_port: u16) -> Result<SocketAddr, NodeWgPeerError> {
        self.validate()?;
        if control_port == 0 {
            return Err(NodeWgPeerError::ControlPort);
        }
        Ok(SocketAddr::new(self.peer_tunnel_address, control_port))
    }
}

fn valid_iface(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..16).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NodeWgPeerError {
    #[error("kernel wireguard needs a listen port or a peer endpoint so either side can dial")]
    NobodyDials,
    #[error("kernel wireguard peer endpoint is invalid")]
    Endpoint,
    #[error("kernel wireguard interface name is invalid")]
    InterfaceName,
    #[error("kernel wireguard private key path is empty")]
    PrivateKeyPath,
    #[error("kernel wireguard tunnel addresses are invalid")]
    TunnelAddress,
    #[error("kernel wireguard peer public key is empty")]
    PeerKey,
    #[error("kernel wireguard control port must be nonzero")]
    ControlPort,
    #[error("kernel wireguard interface was not created: {0}")]
    InterfaceCreate(String),
    #[error("kernel wireguard setup failed: {0}")]
    Setup(String),
}

/// Creates a kernel WireGuard interface for the node↔C2 peer.
/// Allowed IPs and the installed route are only the C2 tunnel host.
/// Does not substitute another transport when `ip link add type wireguard` fails.
pub fn bring_up_node_wireguard(config: &NodeWgPeerConfig) -> Result<IpAddr, NodeWgPeerError> {
    config.validate()?;
    require_key_file(&config.private_key_path)?;
    create_wireguard_iface(&config.interface_name)?;
    if let Err(error) = configure_wireguard(config) {
        delete_iface(&config.interface_name);
        return Err(error);
    }
    Ok(config.peer_tunnel_address)
}

fn require_key_file(path: &Path) -> Result<(), NodeWgPeerError> {
    let meta = std::fs::metadata(path).map_err(|error| {
        NodeWgPeerError::Setup(format!("private key path {}: {error}", path.display()))
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(NodeWgPeerError::Setup(format!(
            "private key path {} is not a nonempty file",
            path.display()
        )));
    }
    Ok(())
}

fn create_wireguard_iface(name: &str) -> Result<(), NodeWgPeerError> {
    let output = Command::new("ip")
        .args(["link", "add", "dev", name, "type", "wireguard"])
        .output()
        .map_err(|error| {
            NodeWgPeerError::InterfaceCreate(format!("ip link add type wireguard: {error}"))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(NodeWgPeerError::InterfaceCreate(format!(
        "ip link add dev {name} type wireguard failed: {stderr}"
    )))
}

fn configure_wireguard(config: &NodeWgPeerConfig) -> Result<(), NodeWgPeerError> {
    let name = &config.interface_name;
    let listen = config.listen_port.to_string();
    let key = config.private_key_path.display().to_string();
    let mut args = vec![
        "set".to_owned(),
        name.clone(),
        "listen-port".to_owned(),
        listen,
        "private-key".to_owned(),
        key,
        "peer".to_owned(),
        config.peer_public_key.clone(),
    ];
    if let Some(endpoint) = config.peer_endpoint {
        args.push("endpoint".to_owned());
        args.push(endpoint.to_string());
    }
    let prefix = match config.peer_tunnel_address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    let allowed = format!("{}/{}", config.peer_tunnel_address, prefix);
    args.push("allowed-ips".to_owned());
    args.push(allowed);
    if config.persistent_keepalive_secs > 0 {
        args.push("persistent-keepalive".to_owned());
        args.push(config.persistent_keepalive_secs.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    command_ok("wg", &arg_refs)?;
    let local_prefix = match config.local_tunnel_address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    let local = format!("{}/{}", config.local_tunnel_address, local_prefix);
    command_ok("ip", &["addr", "replace", &local, "dev", name])?;
    command_ok("ip", &["link", "set", "up", "dev", name])?;
    let route = format!("{}/{}", config.peer_tunnel_address, prefix);
    // Host route to C2 only. Not an HQ route and not a default route.
    command_ok("ip", &["route", "replace", &route, "dev", name])?;
    Ok(())
}

fn command_ok(program: &str, args: &[&str]) -> Result<(), NodeWgPeerError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| NodeWgPeerError::Setup(format!("{program}: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(NodeWgPeerError::Setup(format!(
        "{program} {} failed: {stderr}",
        args.join(" ")
    )))
}

fn delete_iface(name: &str) {
    let _ = Command::new("ip")
        .args(["link", "del", "dev", name])
        .status();
}

/// Optional service-mode bring-up. Unset [`WG_INTERFACE_ENV`] leaves the
/// tunnel down. The key is a path, never a secret value in the unit file.
pub fn node_wg_from_env() -> Result<Option<NodeWgPeerConfig>, String> {
    let interface_name = match std::env::var(WG_INTERFACE_ENV) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(None),
    };
    let private_key_path = PathBuf::from(required_env(WG_PRIVATE_KEY_ENV)?);
    let listen_port = match std::env::var(WG_LISTEN_PORT_ENV) {
        Ok(value) if !value.trim().is_empty() => value
            .parse::<u16>()
            .map_err(|error| format!("{WG_LISTEN_PORT_ENV} is invalid: {error}"))?,
        _ => 51820,
    };
    let local_tunnel_address = parse_ip(WG_ADDRESS_ENV, &required_env(WG_ADDRESS_ENV)?)?;
    let peer_tunnel_address = parse_ip(WG_PEER_ADDRESS_ENV, &required_env(WG_PEER_ADDRESS_ENV)?)?;
    let peer_public_key = required_env(WG_PEER_KEY_ENV)?;
    let peer_endpoint = match std::env::var(WG_PEER_ENDPOINT_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(
            value
                .parse::<SocketAddr>()
                .map_err(|error| format!("{WG_PEER_ENDPOINT_ENV} is invalid: {error}"))?,
        ),
        _ => None,
    };
    let persistent_keepalive_secs = match std::env::var(WG_KEEPALIVE_ENV) {
        Ok(value) if !value.trim().is_empty() => value
            .parse::<u16>()
            .map_err(|error| format!("{WG_KEEPALIVE_ENV} is invalid: {error}"))?,
        _ => 25,
    };
    let config = NodeWgPeerConfig {
        interface_name,
        private_key_path,
        listen_port,
        local_tunnel_address,
        peer_tunnel_address,
        peer_public_key,
        peer_endpoint,
        persistent_keepalive_secs,
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(Some(config))
}

pub const WG_INTERFACE_ENV: &str = "GATE4AGENT_WG_INTERFACE";
pub const WG_PRIVATE_KEY_ENV: &str = "GATE4AGENT_WG_PRIVATE_KEY";
pub const WG_LISTEN_PORT_ENV: &str = "GATE4AGENT_WG_LISTEN_PORT";
pub const WG_ADDRESS_ENV: &str = "GATE4AGENT_WG_ADDRESS";
pub const WG_PEER_ADDRESS_ENV: &str = "GATE4AGENT_WG_PEER_ADDRESS";
pub const WG_PEER_KEY_ENV: &str = "GATE4AGENT_WG_PEER_KEY";
pub const WG_PEER_ENDPOINT_ENV: &str = "GATE4AGENT_WG_PEER_ENDPOINT";
pub const WG_KEEPALIVE_ENV: &str = "GATE4AGENT_WG_KEEPALIVE";

fn required_env(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("{name} is required when {WG_INTERFACE_ENV} is set")),
    }
}

fn parse_ip(name: &str, value: &str) -> Result<IpAddr, String> {
    value
        .parse()
        .map_err(|error| format!("{name} is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dial() -> NodeWgPeerConfig {
        NodeWgPeerConfig {
            interface_name: "g4a-node".to_owned(),
            private_key_path: PathBuf::from("/run/gate4agent/node.key"),
            listen_port: 0,
            local_tunnel_address: "10.88.0.2".parse().unwrap(),
            peer_tunnel_address: "10.88.0.1".parse().unwrap(),
            peer_public_key: "c2-public-key".to_owned(),
            peer_endpoint: Some("203.0.113.10:51820".parse().unwrap()),
            persistent_keepalive_secs: 25,
        }
    }

    #[test]
    fn node_may_dial_c2() {
        assert!(sample_dial().validate().is_ok());
    }

    #[test]
    fn c2_may_dial_node() {
        let mut config = sample_dial();
        config.listen_port = 51820;
        config.peer_endpoint = None;
        config.persistent_keepalive_secs = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn either_side_may_dial_when_both_are_set() {
        let mut config = sample_dial();
        config.listen_port = 51820;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn nobody_dials_is_rejected() {
        let mut config = sample_dial();
        config.listen_port = 0;
        config.peer_endpoint = None;
        assert_eq!(config.validate(), Err(NodeWgPeerError::NobodyDials));
    }

    #[test]
    fn kernel_module_source_is_not_the_mesh_stub() {
        let source = include_str!("kit_wireguard.rs");
        let code = source
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//") && !trimmed.starts_with("assert!")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(code.contains("\"wireguard\""));
        assert!(!code.to_ascii_lowercase().contains("chacha"));
        assert!(!code.contains("HqWg"));
        assert!(!code.contains(concat!("open_wireguard", "_daemon_stub")));
        assert_eq!(LINK_TAG, "kernel-wireguard-node-c2-peer");
    }
}
