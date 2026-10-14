//! The loopback [`Radio`]: the injected frame bytes carried over UDP to a
//! shared-medium relay instead of `AF_PACKET`.
//!
//! Selected at run time with `ADOS_SWARMBUS_TRANSPORT=loopback`, on any OS. It
//! lets several nodes with no monitor-mode adapter (VMs, CI, a dev host) share
//! one emulated channel while every layer above this seam stays production code:
//! the frame layout, the cipher, the neighbour table, the beacon jitter and the
//! service loops all run unchanged.
//!
//! The relay speaks three sockets from one base port:
//!
//! - control, TCP `base`: one JSON line per connection. This backend sends
//!   `{"op":"register","node":..,"channel":..,"direction":"tx"|"rx","port":..}`
//!   (plus `"addr"` when the node's reachable address is configured).
//! - data, UDP `base + 1`: `!I channel, !B name_len, name, frame` per injection.
//! - downlink, UDP to the registered port: `!I channel, frame` per delivery. The
//!   relay never returns a node's own frames to it, as a monitor-mode radio never
//!   hears its own injection.
//!
//! The frame is the exact byte string the `AF_PACKET` backend would inject,
//! radiotap header included. On receive, the classic-BPF program the Linux backend
//! attaches to its socket is run over each delivered frame by
//! [`crate::frame::bpf_run`], so a foreign fleet or foreign magic is dropped here
//! exactly where the kernel would have dropped it.
//!
//! Every fleet shares [`LOOPBACK_CHANNEL`], as every fleet on one RF channel
//! shares the air. Loss, latency and airtime are the relay's business.

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use crate::frame::{bpf_program, bpf_run, SockFilter, BPF_PROGRAM_LEN, MAX_FRAME_LEN, SWARM_MAGIC};

pub use super::loopback_config::{
    LoopbackConfig, DEFAULT_RELAY_HOST, DEFAULT_RELAY_PORT, ENV_LISTEN, ENV_NODE, ENV_NODE_ADDR,
    ENV_RELAY_HOST, ENV_RELAY_PORT, MAX_NODE_LEN,
};

/// The relay routing key every swarm frame travels on.
///
/// The relay keys wfb lanes by `(link_id << 8) + radio_port` with
/// `link_id = (fleet_id << 8) | slot`. This value decodes to slot `0xAD`, far
/// above any legal fleet slot, so no wfb lane can ever share it.
pub const LOOPBACK_CHANNEL: u32 = SWARM_MAGIC as u32;

/// Length of the `!I channel` prefix on every relay datagram.
const CHANNEL_LEN: usize = 4;

/// Bound on one control-socket exchange, so an unreachable relay fails the open
/// promptly and the service's retry loop takes over.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Largest uplink datagram: channel, name length, name, frame.
const MAX_UPLINK_LEN: usize = CHANNEL_LEN + 1 + MAX_NODE_LEN + MAX_FRAME_LEN;

/// Windows `WSAEMSGSIZE`: a datagram larger than the receive buffer.
#[cfg(windows)]
const WSAEMSGSIZE: i32 = 10040;

/// One node's attachment to the relay.
pub struct Radio {
    tx: UdpSocket,
    rx: tokio::net::UdpSocket,
    data: SocketAddr,
    /// `!I channel, !B name_len, name`, the fixed head of every uplink datagram.
    prefix: Vec<u8>,
    filter: [SockFilter; BPF_PROGRAM_LEN],
    iface: String,
}

impl Radio {
    /// Bind the downlink socket, register this node with the relay for
    /// [`LOOPBACK_CHANNEL`] in both directions, and filter receive to `fleet_id`.
    ///
    /// Fails when the relay host does not resolve, the downlink address cannot be
    /// bound, or the relay's control port does not accept the registration; the
    /// service retries.
    ///
    /// # Panics
    ///
    /// MUST be called from inside a Tokio runtime, like the Linux backend: the
    /// receive half registers with the reactor.
    pub fn open(cfg: &LoopbackConfig, fleet_id: u16) -> io::Result<Self> {
        let control = resolve(&cfg.relay_host, cfg.relay_port)?;
        let data = resolve(&cfg.relay_host, cfg.relay_port.wrapping_add(1))?;
        let rx = UdpSocket::bind(cfg.listen)?;
        rx.set_nonblocking(true)?;
        let port = rx.local_addr()?.port();
        let tx = UdpSocket::bind(send_bind_addr(data))?;
        tx.set_nonblocking(true)?;
        for direction in ["tx", "rx"] {
            register(control, &register_line(cfg, direction, port)).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("swarm loopback relay control at {control}: {e}"),
                )
            })?;
        }
        Ok(Self {
            tx,
            rx: tokio::net::UdpSocket::from_std(rx)?,
            data,
            prefix: uplink_prefix(&cfg.node),
            filter: bpf_program(fleet_id),
            iface: format!("loopback:{}", cfg.node),
        })
    }

    /// `loopback:<node>`, reported where the Linux backend reports its interface.
    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// Send one complete frame to the relay's data port. Returns the frame length
    /// on success, matching the `AF_PACKET` backend.
    pub fn send(&self, frame: &[u8]) -> io::Result<usize> {
        let mut buf = [0u8; MAX_UPLINK_LEN];
        let n = encode_uplink(&self.prefix, frame, &mut buf)?;
        let sent = self.tx.send_to(&buf[..n], self.data)?;
        if sent != n {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short relay datagram: {sent} of {n} bytes"),
            ));
        }
        Ok(frame.len())
    }

    /// Await one delivered frame that passes the fleet filter, returning its
    /// length. Datagrams on another channel, too short to carry a channel, or
    /// rejected by the filter are skipped.
    pub async fn recv(&self, buf: &mut [u8; MAX_FRAME_LEN]) -> io::Result<usize> {
        let mut dg = [0u8; CHANNEL_LEN + MAX_FRAME_LEN];
        loop {
            let n = match self.rx.recv_from(&mut dg).await {
                Ok((n, _peer)) => n,
                Err(e) if skippable(&e) => continue,
                Err(e) => return Err(e),
            };
            let Some(frame) = downlink_frame(&dg[..n]) else {
                continue;
            };
            let kept = bpf_run(&self.filter, frame);
            if kept == 0 {
                continue;
            }
            buf[..kept].copy_from_slice(&frame[..kept]);
            return Ok(kept);
        }
    }
}

/// Resolve `host:port`, preferring IPv4 as the relay does.
fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("swarm loopback relay host {host} resolves to no address"),
            )
        })
}

/// The local address the send socket binds: loopback for a loopback relay, so a
/// single-host setup never opens a socket on an external interface.
fn send_bind_addr(data: SocketAddr) -> SocketAddr {
    let ip = match data.ip() {
        ip if ip.is_loopback() => ip,
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    SocketAddr::new(ip, 0)
}

/// One control exchange: connect, write the line, close.
fn register(control: SocketAddr, line: &str) -> io::Result<()> {
    let mut conn = TcpStream::connect_timeout(&control, CONTROL_TIMEOUT)?;
    conn.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    conn.write_all(line.as_bytes())?;
    conn.flush()
}

/// The `register` control line for one direction, newline-terminated.
fn register_line(cfg: &LoopbackConfig, direction: &str, port: u16) -> String {
    let mut req = serde_json::json!({
        "op": "register",
        "node": cfg.node,
        "channel": LOOPBACK_CHANNEL,
        "direction": direction,
        "port": port,
    });
    if let Some(addr) = &cfg.node_addr {
        req["addr"] = serde_json::Value::String(addr.clone());
    }
    format!("{req}\n")
}

/// `!I channel, !B name_len, name`. The name is bounded by [`LoopbackConfig`].
fn uplink_prefix(node: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(CHANNEL_LEN + 1 + node.len());
    p.extend_from_slice(&LOOPBACK_CHANNEL.to_be_bytes());
    p.push(node.len() as u8);
    p.extend_from_slice(node.as_bytes());
    p
}

/// Write `prefix || frame` into `out`, returning the datagram length.
fn encode_uplink(prefix: &[u8], frame: &[u8], out: &mut [u8]) -> io::Result<usize> {
    if frame.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame of {} bytes exceeds {MAX_FRAME_LEN}", frame.len()),
        ));
    }
    let n = prefix.len() + frame.len();
    let dst = out
        .get_mut(..n)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    dst[..prefix.len()].copy_from_slice(prefix);
    dst[prefix.len()..].copy_from_slice(frame);
    Ok(n)
}

/// The frame inside a downlink datagram, when it is on [`LOOPBACK_CHANNEL`].
fn downlink_frame(dg: &[u8]) -> Option<&[u8]> {
    let (channel, frame) = dg.split_first_chunk::<CHANNEL_LEN>()?;
    (u32::from_be_bytes(*channel) == LOOPBACK_CHANNEL).then_some(frame)
}

/// Receive errors that concern one datagram, not the socket.
fn skippable(e: &io::Error) -> bool {
    #[cfg(windows)]
    if e.raw_os_error() == Some(WSAEMSGSIZE) {
        return true;
    }
    // A Windows UDP socket reports an earlier ICMP port-unreachable this way.
    e.kind() == io::ErrorKind::ConnectionReset
}

#[cfg(test)]
#[path = "loopback_tests.rs"]
mod tests;
