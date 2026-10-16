//! Windows node↔C2 tunnel.
//!
//! - no tun owns the default route: open one adapter, host route for the peer only;
//! - a tun already owns it: do not open an adapter and do not add a route.
//!   WireGuard runs in this process. Its UDP leaves through the existing tun.
//!
//! There is no default route and no edit of any other adapter.

#[path = "kit_wireguard_inside.rs"]
mod inside;

use std::ffi::CString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_UNICAST_IF, SOCKET};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::kit_wireguard::{NodeWgPeerConfig, NodeWgPeerError};

const RING: u32 = 0x20000;
const MTU: u16 = 1280;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefaultRoute {
    pub if_index: u32,
    pub metric: u32,
    pub tun: bool,
    pub alias: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    Open,
    InsideExisting { if_index: u32, alias: String },
}

pub(crate) fn classify(routes: &[DefaultRoute]) -> Placement {
    let Some(best) = routes.iter().min_by_key(|route| route.metric) else {
        return Placement::Open;
    };
    if best.tun {
        Placement::InsideExisting {
            if_index: best.if_index,
            alias: best.alias.clone(),
        }
    } else {
        Placement::Open
    }
}

pub(crate) fn peer_prefix(peer: IpAddr) -> String {
    match peer {
        IpAddr::V4(ip) => format!("{ip}/32"),
        IpAddr::V6(ip) => format!("{ip}/128"),
    }
}

pub(crate) fn opens_adapter(placement: &Placement) -> bool {
    matches!(placement, Placement::Open)
}

pub(crate) fn connect_inside(addr: SocketAddr) -> std::io::Result<Option<std::net::TcpStream>> {
    inside::connect(addr)
}

pub(crate) fn bring_up(config: &NodeWgPeerConfig) -> Result<(), NodeWgPeerError> {
    let placement = read_default_routes().unwrap_or_else(|error| {
        eprintln!("wireguard: default-route lookup failed ({error}); opening a standalone tun");
        Vec::new()
    });
    let placement = classify(&placement);
    if let Placement::InsideExisting { alias, if_index } = &placement {
        if alias.eq_ignore_ascii_case(&config.interface_name) {
            return Err(NodeWgPeerError::Setup(
                "refusing to adopt the tun that already owns the default route".to_owned(),
            ));
        }
        let (secret, peer_key) = load_keys(&config.private_key_path, &config.peer_public_key)
            .map_err(NodeWgPeerError::Setup)?;
        eprintln!("wireguard: variant=inside-existing exit={alias} if={if_index} adapter=none");
        inside::start(config, secret, peer_key, *if_index).map_err(NodeWgPeerError::Setup)?;
        return Ok(());
    }
    let (secret, peer_key) = load_keys(&config.private_key_path, &config.peer_public_key)
        .map_err(NodeWgPeerError::Setup)?;
    let mut live = LiveTun::open(&config.interface_name).map_err(NodeWgPeerError::InterfaceCreate)?;
    if let Err(error) = assign_host_route(&config.interface_name, config.local_tunnel_address, config.peer_tunnel_address)
    {
        drop(live);
        return Err(NodeWgPeerError::Setup(error));
    }
    let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.listen_port)))
        .map_err(|error| NodeWgPeerError::Setup(format!("udp bind: {error}")))?;
    udp.set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|error| NodeWgPeerError::Setup(format!("udp timeout: {error}")))?;
    if let Placement::InsideExisting { if_index, alias } = &placement {
        bind_v4_exit(&udp, *if_index).map_err(NodeWgPeerError::Setup)?;
        eprintln!(
            "wireguard: variant=inside-existing exit={alias} if={if_index} iface={} route={}",
            config.interface_name,
            peer_prefix(config.peer_tunnel_address)
        );
    } else {
        eprintln!(
            "wireguard: variant=open iface={} route={}",
            config.interface_name,
            peer_prefix(config.peer_tunnel_address)
        );
    }
    let index = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);
    let tunn = Tunn::new(
        secret,
        peer_key,
        None,
        (config.persistent_keepalive_secs > 0).then_some(config.persistent_keepalive_secs),
        index,
        None,
    );
    let endpoint = config.peer_endpoint;
    live.session = unsafe { (live.api.start)(live.adapter, RING) };
    if live.session.is_null() {
        let err = unsafe { GetLastError() };
        drop(live);
        return Err(NodeWgPeerError::Setup(format!("wintun session failed ({err})")));
    }
    let iface = config.interface_name.clone();
    std::thread::Builder::new()
        .name(format!("{iface}-wg"))
        .spawn(move || pump(live, tunn, udp, endpoint))
        .map_err(|error| NodeWgPeerError::Setup(format!("wireguard thread: {error}")))?;
    Ok(())
}

fn assign_host_route(iface: &str, local: IpAddr, peer: IpAddr) -> Result<(), String> {
    match (local, peer) {
        (IpAddr::V4(local), IpAddr::V4(peer)) => {
            run_netsh(&[
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={iface}"),
                "source=static",
                &format!("address={local}"),
                "mask=255.255.255.255",
                "gateway=none",
            ])?;
            let _ = run_netsh(&[
                "interface",
                "ipv4",
                "set",
                "subinterface",
                iface,
                &format!("mtu={MTU}"),
                "store=active",
            ]);
            let prefix = format!("{peer}/32");
            let _ = run_netsh(&[
                "interface",
                "ipv4",
                "delete",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={iface}"),
                "store=active",
            ]);
            run_netsh(&[
                "interface",
                "ipv4",
                "add",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={iface}"),
                "metric=0",
                "store=active",
            ])?;
            // `set address` resets the interface metric. Set it last, or the
            // other tun's copy of the same /32 ties and wins.
            run_netsh(&[
                "interface",
                "ipv4",
                "set",
                "interface",
                &format!("name={iface}"),
                "metric=1",
            ])?;
            Ok(())
        }
        (IpAddr::V6(local), IpAddr::V6(peer)) => {
            run_netsh(&[
                "interface",
                "ipv6",
                "set",
                "address",
                &format!("interface={iface}"),
                &format!("address={local}"),
            ])?;
            let prefix = format!("{peer}/128");
            run_netsh(&[
                "interface",
                "ipv6",
                "add",
                "route",
                &format!("prefix={prefix}"),
                &format!("interface={iface}"),
                "metric=0",
                "store=active",
            ])?;
            Ok(())
        }
        _ => Err("tunnel addresses are not the same family".to_owned()),
    }
}

fn run_netsh(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|arg| arg.contains("0.0.0.0/0") || arg.contains("::/0")) {
        return Err("refusing a default route".to_owned());
    }
    let output = Command::new("netsh").args(args).output().map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!("netsh {} failed: {stderr} {stdout}", args.join(" ")))
}

fn bind_v4_exit(udp: &UdpSocket, if_index: u32) -> Result<(), String> {
    let sock = udp.as_raw_socket() as SOCKET;
    let value = if_index.to_be();
    let rc = unsafe {
        setsockopt(
            sock,
            IPPROTO_IP,
            IP_UNICAST_IF,
            &value as *const u32 as *const u8,
            4,
        )
    };
    if rc != 0 {
        return Err(format!("handshake egress to existing tun if {if_index} failed ({rc})"));
    }
    Ok(())
}

fn read_default_routes() -> Result<Vec<DefaultRoute>, String> {
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | ForEach-Object {
  $a = Get-NetAdapter -InterfaceIndex $_.InterfaceIndex -ErrorAction SilentlyContinue
  $desc = ''
  if ($null -ne $a) { $desc = [string]$a.InterfaceDescription }
  $alias = [string]$_.InterfaceAlias
  $blob = ($alias + ' ' + $desc).ToLowerInvariant()
  $tun = 0
  foreach ($n in @('wintun','wireguard','amnezia','tailscale','tap-windows')) {
    if ($blob.Contains($n)) { $tun = 1 }
  }
  $safe = if ($alias -match '^[A-Za-z0-9 _\-\.\(\)]+$') { $alias } else { '-' }
  Write-Output ('{0} {1} {2} {3}' -f [int]$_.InterfaceIndex, [int]$_.RouteMetric, $tun, $safe)
}
"#;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(parse_route_lines(&stdout_text(&output.stdout)))
}

fn stdout_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && (bytes.starts_with(&[0xFF, 0xFE]) || bytes[1] == 0) {
        let start = if bytes.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 };
        let units: Vec<u16> = bytes[start..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn parse_route_lines(text: &str) -> Vec<DefaultRoute> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.split_whitespace();
            let if_index = parts.next()?.parse().ok()?;
            let metric = parts.next()?.parse().ok()?;
            let tun = parts.next()? == "1";
            let alias = {
                let rest: Vec<&str> = parts.collect();
                if rest.is_empty() {
                    "-".to_owned()
                } else {
                    rest.join(" ")
                }
            };
            Some(DefaultRoute {
                if_index,
                metric,
                tun,
                alias,
            })
        })
        .collect()
}

fn load_keys(path: &Path, peer_b64: &str) -> Result<(StaticSecret, PublicKey), String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let ours = decode_key(text.trim())?;
    let peer = decode_key(peer_b64.trim())?;
    Ok((StaticSecret::from(ours), PublicKey::from(peer)))
}

fn decode_key(text: &str) -> Result<[u8; 32], String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text))
        .map_err(|error| format!("wireguard key: {error}"))?;
    let array: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("wireguard key is {} bytes", bytes.len()))?;
    Ok(array)
}

type CreateAdapter = unsafe extern "system" fn(*const u16, *const u16, *const u8) -> *mut core::ffi::c_void;
type CloseAdapter = unsafe extern "system" fn(*mut core::ffi::c_void);
type OpenAdapter = unsafe extern "system" fn(*const u16) -> *mut core::ffi::c_void;
type StartSession = unsafe extern "system" fn(*mut core::ffi::c_void, u32) -> *mut core::ffi::c_void;
type EndSession = unsafe extern "system" fn(*mut core::ffi::c_void);
type ReceivePacket = unsafe extern "system" fn(*mut core::ffi::c_void, *mut u32) -> *mut u8;
type ReleaseReceive = unsafe extern "system" fn(*mut core::ffi::c_void, *const u8);
type AllocateSend = unsafe extern "system" fn(*mut core::ffi::c_void, u32) -> *mut u8;
type SendPacket = unsafe extern "system" fn(*mut core::ffi::c_void, *const u8);

struct TunApi {
    create: CreateAdapter,
    close: CloseAdapter,
    open: OpenAdapter,
    start: StartSession,
    end: EndSession,
    recv: ReceivePacket,
    release: ReleaseReceive,
    alloc: AllocateSend,
    send: SendPacket,
}

struct LiveTun {
    api: TunApi,
    adapter: *mut core::ffi::c_void,
    session: *mut core::ffi::c_void,
}

unsafe impl Send for LiveTun {}

impl Drop for LiveTun {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                (self.api.end)(self.session);
                self.session = std::ptr::null_mut();
            }
            if !self.adapter.is_null() {
                (self.api.close)(self.adapter);
                self.adapter = std::ptr::null_mut();
            }
        }
    }
}

impl LiveTun {
    fn open(name: &str) -> Result<Self, String> {
        let api = load_api()?;
        let name_w = wide(name);
        let kind = wide("Hatchery");
        unsafe {
            let existing = (api.open)(name_w.as_ptr());
            if !existing.is_null() {
                (api.close)(existing);
            }
            let adapter = (api.create)(name_w.as_ptr(), kind.as_ptr(), std::ptr::null());
            if adapter.is_null() {
                let err = GetLastError();
                let hint = if err == 5 {
                    " (the process needs elevation, same as Linux CAP_NET_ADMIN)"
                } else {
                    ""
                };
                return Err(format!("wintun create {name} failed ({err}){hint}"));
            }
            Ok(Self {
                api,
                adapter,
                session: std::ptr::null_mut(),
            })
        }
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        let mut size = 0u32;
        unsafe {
            let ptr = (self.api.recv)(self.session, &mut size);
            if ptr.is_null() || size == 0 || size > 2048 {
                if !ptr.is_null() {
                    (self.api.release)(self.session, ptr);
                }
                return None;
            }
            let packet = std::slice::from_raw_parts(ptr, size as usize).to_vec();
            (self.api.release)(self.session, ptr);
            Some(packet)
        }
    }

    fn send(&self, packet: &[u8]) {
        if packet.is_empty() || packet.len() > 2048 {
            return;
        }
        unsafe {
            let dst = (self.api.alloc)(self.session, packet.len() as u32);
            if dst.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(packet.as_ptr(), dst, packet.len());
            (self.api.send)(self.session, dst);
        }
    }
}

fn load_api() -> Result<TunApi, String> {
    let path = find_dll()?;
    let wide_path = wide(&path);
    unsafe {
        let lib = LoadLibraryW(wide_path.as_ptr());
        if lib.is_null() {
            return Err(format!("LoadLibrary {path} failed ({})", GetLastError()));
        }
        Ok(TunApi {
            create: sym(lib, "WintunCreateAdapter")?,
            close: sym(lib, "WintunCloseAdapter")?,
            open: sym(lib, "WintunOpenAdapter")?,
            start: sym(lib, "WintunStartSession")?,
            end: sym(lib, "WintunEndSession")?,
            recv: sym(lib, "WintunReceivePacket")?,
            release: sym(lib, "WintunReleaseReceivePacket")?,
            alloc: sym(lib, "WintunAllocateSendPacket")?,
            send: sym(lib, "WintunSendPacket")?,
        })
    }
}

fn find_dll() -> Result<String, String> {
    let mut candidates = Vec::new();
    if let Some(from_env) = std::env::var_os("HATCHERY_WINTUN_DLL") {
        candidates.push(std::path::PathBuf::from(from_env));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("wintun.dll"));
        }
    }
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        let root = std::path::PathBuf::from(programs);
        candidates.push(root.join("WireGuard").join("wintun.dll"));
    }
    for path in &candidates {
        if path.is_file() {
            return Ok(path.display().to_string());
        }
    }
    Err("wintun.dll not found next to the exe or in HATCHERY_WINTUN_DLL".to_owned())
}

unsafe fn sym<T>(lib: HMODULE, name: &str) -> Result<T, String> {
    let c = CString::new(name).map_err(|error| error.to_string())?;
    let far = GetProcAddress(lib, c.as_ptr() as *const u8).ok_or_else(|| format!("wintun.dll missing {name}"))?;
    Ok(std::mem::transmute_copy(&far))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn pump(live: LiveTun, mut tunn: Tunn, udp: UdpSocket, endpoint: Option<SocketAddr>) {
    let mut net_buf = [0u8; 2048];
    let mut tun_buf = [0u8; 2048];
    loop {
        if let Some(dst) = endpoint {
            send_network(&udp, dst, tunn.update_timers(&mut net_buf));
        }
        if let Some(packet) = live.try_recv() {
            send_network(&udp, endpoint.unwrap_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))), {
                // unwrap_or is only used when endpoint is None; encapsulate still returns WriteToNetwork
                // and send_network drops a send to port 0.
                tunn.encapsulate(&packet, &mut net_buf)
            });
        }
        let mut recvd = [0u8; 2048];
        match udp.recv_from(&mut recvd) {
            Ok((n, src)) => {
                let mut datagram = recvd[..n].to_vec();
                loop {
                    let outcome = tunn.decapsulate(
                        Some(src.ip()),
                        &datagram,
                        &mut tun_buf,
                    );
                    datagram.clear();
                    match outcome {
                        TunnResult::WriteToNetwork(packet) => {
                            let copy = packet.to_vec();
                            let _ = udp.send_to(&copy, src);
                        }
                        TunnResult::WriteToTunnelV4(packet, _) | TunnResult::WriteToTunnelV6(packet, _) => {
                            live.send(packet);
                        }
                        TunnResult::Done | TunnResult::Err(_) => break,
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn send_network(udp: &UdpSocket, dst: SocketAddr, result: TunnResult<'_>) {
    if dst.port() == 0 {
        return;
    }
    if let TunnResult::WriteToNetwork(packet) = result {
        let _ = udp.send_to(packet, dst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_opens_when_the_default_route_is_not_a_tun() {
        let routes = vec![DefaultRoute {
            if_index: 5,
            metric: 25,
            tun: false,
            alias: "eth0".to_owned(),
        }];
        assert_eq!(classify(&routes), Placement::Open);
    }

    #[test]
    fn classify_uses_the_lowest_metric_tun_and_ignores_a_higher_physical_route() {
        let routes = vec![
            DefaultRoute {
                if_index: 12,
                metric: 256,
                tun: false,
                alias: "eth0".to_owned(),
            },
            DefaultRoute {
                if_index: 40,
                metric: 0,
                tun: true,
                alias: "tun0".to_owned(),
            },
        ];
        assert_eq!(
            classify(&routes),
            Placement::InsideExisting {
                if_index: 40,
                alias: "tun0".to_owned(),
            }
        );
    }

    #[test]
    fn inside_existing_does_not_open_an_adapter() {
        let placement = Placement::InsideExisting {
            if_index: 7,
            alias: "tun0".to_owned(),
        };
        assert!(!opens_adapter(&placement));
        assert!(opens_adapter(&Placement::Open));
    }

    #[test]
    fn peer_prefix_is_a_host_route() {
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(peer_prefix(v4), "192.0.2.1/32");
        assert!(!peer_prefix(v4).contains("0.0.0.0/0"));
    }

    #[test]
    fn route_lines_parse_alias_with_spaces() {
        let routes = parse_route_lines("15 0 1 nic 2\n9 256 0 nic0\n");
        assert_eq!(routes[0].alias, "nic 2");
        assert!(routes[0].tun);
        assert!(!routes[1].tun);
    }
}
