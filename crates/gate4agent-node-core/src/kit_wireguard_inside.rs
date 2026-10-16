//! WireGuard inside an already-up tun. No adapter and no route are created.
//! Ciphertext UDP leaves through that tun. TCP to the peer tunnel address
//! is terminated in this process.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{sync_channel, SyncSender, TryRecvError};
use std::sync::OnceLock;
use std::time::{Duration, Instant as StdInstant};

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use crate::kit_wireguard::NodeWgPeerConfig;

use super::bind_v4_exit;

struct Gate {
    peer: IpAddr,
    jobs: SyncSender<Job>,
}

struct Job {
    port: u16,
    listener: TcpListener,
}

static GATE: OnceLock<Gate> = OnceLock::new();

/// Idle time before the inner (in-tunnel) TCP socket sends a keepalive.
const INNER_KEEPALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(15);
/// Unacked time after which the inner TCP socket is reset.
const INNER_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(45);

pub(super) fn connect(addr: SocketAddr) -> std::io::Result<Option<TcpStream>> {
    let Some(gate) = GATE.get() else {
        return Ok(None);
    };
    if addr.ip() != gate.peer {
        return Ok(None);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    gate.jobs
        .send(Job {
            port: addr.port(),
            listener,
        })
        .map_err(|_| std::io::Error::other("wireguard session is gone"))?;
    let stream = TcpStream::connect_timeout(&local, Duration::from_secs(20))?;
    stream.set_nodelay(true)?;
    Ok(Some(stream))
}

pub(super) fn start(
    config: &NodeWgPeerConfig,
    secret: StaticSecret,
    peer_key: PublicKey,
    exit_if: u32,
) -> Result<(), String> {
    let IpAddr::V4(local) = config.local_tunnel_address else {
        return Err("inside-existing tunnel is ipv4 only".to_owned());
    };
    let IpAddr::V4(peer) = config.peer_tunnel_address else {
        return Err("inside-existing tunnel is ipv4 only".to_owned());
    };
    let endpoint = config
        .peer_endpoint
        .ok_or_else(|| "inside-existing tunnel needs the peer endpoint".to_owned())?;
    let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.listen_port)))
        .map_err(|error| format!("udp bind: {error}"))?;
    udp.set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|error| format!("udp timeout: {error}"))?;
    bind_v4_exit(&udp, exit_if)?;
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
    let (tx, rx) = sync_channel(8);
    let gate = Gate {
        peer: config.peer_tunnel_address,
        jobs: tx,
    };
    if GATE.set(gate).is_err() {
        return Err("wireguard session is already up".to_owned());
    }
    let keepalive = config.persistent_keepalive_secs;
    std::thread::Builder::new()
        .name("wg-inside".to_owned())
        .spawn(move || pump(local, peer, endpoint, udp, tunn, rx, keepalive))
        .map_err(|error| format!("wireguard thread: {error}"))?;
    let _ = keepalive;
    Ok(())
}

struct Link {
    handle: smoltcp::iface::SocketHandle,
    local: TcpStream,
    to_wg: VecDeque<u8>,
    to_local: VecDeque<u8>,
    opened: StdInstant,
}

struct IpDev {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct DevRx(Vec<u8>);

impl RxToken for DevRx {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.0)
    }
}

struct DevTx<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for DevTx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let out = f(&mut buf);
        self.0.push_back(buf);
        out
    }
}

impl Device for IpDev {
    type RxToken<'a> = DevRx;
    type TxToken<'a> = DevTx<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.rx.pop_front()?;
        Some((DevRx(packet), DevTx(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(DevTx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 1280;
        caps
    }
}

fn pump(
    local: Ipv4Addr,
    peer: Ipv4Addr,
    endpoint: SocketAddr,
    udp: UdpSocket,
    mut tunn: Tunn,
    jobs: std::sync::mpsc::Receiver<Job>,
    _keepalive: u16,
) {
    let mut device = IpDev {
        rx: VecDeque::new(),
        tx: VecDeque::new(),
    };
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = 0xC2_5A_11;
    let mut iface = Interface::new(config, &mut device, Instant::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(local.into()), 32));
    });
    let _ = iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::from(peer));
    let mut sockets = SocketSet::new(vec![]);
    let mut links: Vec<Link> = Vec::new();
    let mut pending: VecDeque<Job> = VecDeque::new();
    let mut next_port: u16 = 20_000;
    let mut net_buf = [0u8; 2048];
    let mut tun_buf = [0u8; 2048];
    let mut udp_out = 0u64;
    let mut udp_in = 0u64;
    let mut send_fail = 0u64;
    let mut last_err = 0i32;
    let mut reported = StdInstant::now();
    note_send(
        &mut udp_out,
        &mut send_fail,
        &mut last_err,
        send_out(&udp, endpoint, tunn.format_handshake_initiation(&mut net_buf, false)),
    );
    loop {
        loop {
            match jobs.try_recv() {
                Ok(job) => pending.push_back(job),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        note_send(
            &mut udp_out,
            &mut send_fail,
            &mut last_err,
            send_out(&udp, endpoint, tunn.update_timers(&mut net_buf)),
        );
        let mut recvd = [0u8; 2048];
        match udp.recv_from(&mut recvd) {
            Ok((n, src)) => {
                let mut datagram = recvd[..n].to_vec();
                loop {
                    let outcome = tunn.decapsulate(Some(src.ip()), &datagram, &mut tun_buf);
                    datagram.clear();
                    match outcome {
                        TunnResult::WriteToNetwork(packet) => {
                            let copy = packet.to_vec();
                            note_send(
                                &mut udp_out,
                                &mut send_fail,
                                &mut last_err,
                                match udp.send_to(&copy, src) {
                                    Ok(_) => 1,
                                    Err(error) => -error.raw_os_error().unwrap_or(1),
                                },
                            );
                        }
                        TunnResult::WriteToTunnelV4(packet, _) | TunnResult::WriteToTunnelV6(packet, _) => {
                            udp_in = udp_in.saturating_add(1);
                            device.rx.push_back(packet.to_vec());
                        }
                        TunnResult::Done | TunnResult::Err(_) => break,
                    }
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
        while let Some(job) = pending.pop_front() {
            match job.listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_nodelay(true);
                    let handle = sockets.add(tcp::Socket::new(
                        tcp::SocketBuffer::new(vec![0u8; 65_535]),
                        tcp::SocketBuffer::new(vec![0u8; 65_535]),
                    ));
                    let local_port = next_port;
                    next_port = next_port.wrapping_add(1).max(20_000);
                    let connected = {
                        let socket = sockets.get_mut::<tcp::Socket>(handle);
                        // The loopback leg cannot see the tunnel die, so the
                        // inner socket must: probe when idle, give up when
                        // the peer stops acking. Its abort closes the loopback
                        // stream and the caller's reconnect logic runs.
                        socket.set_keep_alive(Some(INNER_KEEPALIVE));
                        socket.set_timeout(Some(INNER_TIMEOUT));
                        socket
                            .connect(
                                iface.context(),
                                (IpAddress::Ipv4(peer.into()), job.port),
                                local_port,
                            )
                            .is_ok()
                    };
                    if connected {
                        links.push(Link {
                            handle,
                            local: stream,
                            to_wg: VecDeque::new(),
                            to_local: VecDeque::new(),
                            opened: StdInstant::now(),
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    pending.push_front(job);
                    break;
                }
                Err(_) => {}
            }
        }
        let _ = iface.poll(Instant::now(), &mut device, &mut sockets);
        while let Some(packet) = device.tx.pop_front() {
            note_send(
                &mut udp_out,
                &mut send_fail,
                &mut last_err,
                send_out(&udp, endpoint, tunn.encapsulate(&packet, &mut net_buf)),
            );
        }
        if reported.elapsed() >= Duration::from_secs(5) {
            if tunn.time_since_last_handshake().is_none() {
                note_send(
                    &mut udp_out,
                    &mut send_fail,
                    &mut last_err,
                    send_out(
                        &udp,
                        endpoint,
                        tunn.format_handshake_initiation(&mut net_buf, true),
                    ),
                );
            }
            let inner = links.len();
            let session = u8::from(tunn.time_since_last_handshake().is_some());
            eprintln!(
                "wireguard: inside udp_out={udp_out} udp_in={udp_in} send_fail={send_fail} last_err={last_err} session={session} streams={inner}"
            );
            reported = StdInstant::now();
        }
        let mut keep = Vec::new();
        for mut link in links.drain(..) {
            let mut tmp = [0u8; 1500];
            if link.to_wg.len() < 1_048_576 {
                match link.local.read(&mut tmp) {
                    Ok(0) => {
                        sockets.get_mut::<tcp::Socket>(link.handle).close();
                    }
                    Ok(n) => link.to_wg.extend(tmp[..n].iter().copied()),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        sockets.get_mut::<tcp::Socket>(link.handle).abort();
                        continue;
                    }
                }
            }
            {
                let socket = sockets.get_mut::<tcp::Socket>(link.handle);
                if socket.may_send() && !link.to_wg.is_empty() {
                    let buf: Vec<u8> = link.to_wg.iter().copied().take(2048).collect();
                    if let Ok(n) = socket.send_slice(&buf) {
                        link.to_wg.drain(..n);
                    }
                }
                if socket.may_recv() {
                    if let Ok(n) = socket.recv_slice(&mut tmp) {
                        link.to_local.extend(tmp[..n].iter().copied());
                    }
                }
                if link.opened.elapsed() > Duration::from_secs(20) && !socket.may_send() && link.to_local.is_empty()
                {
                    socket.abort();
                    continue;
                }
                if !socket.is_active() && link.to_local.is_empty() && link.to_wg.is_empty() {
                    continue;
                }
            }
            if !link.to_local.is_empty() {
                let buf: Vec<u8> = link.to_local.iter().copied().collect();
                match link.local.write(&buf) {
                    Ok(0) => {
                        sockets.get_mut::<tcp::Socket>(link.handle).abort();
                        continue;
                    }
                    Ok(n) => {
                        link.to_local.drain(..n);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        sockets.get_mut::<tcp::Socket>(link.handle).abort();
                        continue;
                    }
                }
            }
            keep.push(link);
        }
        links = keep;
        let _ = local;
    }
}

fn note_send(udp_out: &mut u64, send_fail: &mut u64, last_err: &mut i32, rc: i32) {
    if rc > 0 {
        *udp_out = udp_out.saturating_add(1);
    } else if rc < 0 {
        *send_fail = send_fail.saturating_add(1);
        *last_err = -rc;
    }
}

fn send_out(udp: &UdpSocket, dst: SocketAddr, result: TunnResult<'_>) -> i32 {
    if dst.port() == 0 {
        return 0;
    }
    if let TunnResult::WriteToNetwork(packet) = result {
        return match udp.send_to(packet, dst) {
            Ok(_) => 1,
            Err(error) => -error.raw_os_error().unwrap_or(1),
        };
    }
    0
}
