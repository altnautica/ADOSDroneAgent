//! FC transport: serial discovery + baud probe, TCP/UDP for SITL, and the
//! low-level write/persist helpers shared by the connection FSM.
//!
//! A duplex MAVLink byte transport is serial, TCP, or (datagram) UDP. Reading
//! and writing go through the boxed [`AsyncRead`]/[`AsyncWrite`] halves below, so
//! the run loop is transport-agnostic. Serial is the default on the dev rigs;
//! the `tcp:`/`udp:` connection strings exist for SITL.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::{self};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::UdpSocket;
use tokio_serial::{SerialPortBuilderExt, SerialPortType, SerialStream};

use super::framing::{count_msp_frame_starts, extract_frames};

/// A duplex MAVLink byte transport: serial, TCP, or (datagram) UDP. Reading and
/// writing go through the boxed [`AsyncRead`]/[`AsyncWrite`] halves below, so the
/// run loop is transport-agnostic. Serial is the default on the dev rigs; the
/// `tcp:`/`udp:` connection strings exist for SITL.
pub(crate) type BoxedReadHalf = Pin<Box<dyn AsyncRead + Send + Unpin>>;
pub(crate) type BoxedWriteHalf = Pin<Box<dyn AsyncWrite + Send + Unpin>>;

/// Wraps a connected [`UdpSocket`] as a duplex byte stream. MAVLink datagrams
/// arrive one frame per packet, so a read yields one datagram's bytes and a
/// write sends one datagram. The socket is connected to the peer first so plain
/// `send`/`recv` reach it.
pub(crate) struct UdpAdapter {
    pub(crate) sock: std::sync::Arc<UdpSocket>,
}

impl AsyncRead for UdpAdapter {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.sock.poll_recv(cx, buf)
    }
}

impl AsyncWrite for UdpAdapter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.sock.poll_send(cx, data)
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Serial device name prefixes scanned when no explicit port is configured.
const SERIAL_PREFIXES: &[&str] = &[
    "/dev/ttyACM",
    "/dev/ttyAMA",
    "/dev/ttyUSB",
    "/dev/ttyS",
    "/dev/tty.usbmodem",
    "/dev/tty.usbserial",
];

/// Baud rates probed in order (most-common-first so the usual case early-outs
/// fast), falling back to [`BAUD_FALLBACK`] when none yields a HEARTBEAT. 115200
/// is the common USB-CDC / UART default, 921600 the high-rate telemetry UART,
/// 57600 the legacy radio default; the tail covers the less common rates a
/// UART-attached FC may use. A USB-CDC ACM device ignores the requested baud
/// (bytes flow at native USB rate), so widening this list helps real-UART FCs,
/// not a USB-VCP board — for that, the MSP sniff in [`probe_baud`] is the signal.
pub(crate) const BAUD_CANDIDATES: &[u32] = &[115200, 921600, 57600, 230400, 1500000, 38400, 19200];
pub(crate) const BAUD_FALLBACK: u32 = 57600;

/// How long to listen for an FC heartbeat (or MSP traffic) at each candidate
/// baud. A real ArduPilot/PX4 HEARTBEAT is 1 Hz, so this window catches one with
/// margin while bounding the worst-case sweep over the candidate list.
const PROBE_WINDOW: Duration = Duration::from_millis(1500);

/// The outcome of probing a single candidate baud.
pub(crate) enum ProbeOutcome {
    /// A MAVLink HEARTBEAT decoded — this baud is the live FC link.
    Heartbeat,
    /// MSP frame starts were seen (and no HEARTBEAT). The FC is emitting MSP, not
    /// MAVLink, so no baud will ever yield a HEARTBEAT — the caller stops the
    /// sweep and opens here so the read loop surfaces the `msp_detected` hint.
    Msp,
    /// Neither a HEARTBEAT nor MSP traffic within the window.
    None,
}

/// Current ISO-8601 UTC timestamp, e.g. `2026-05-28T15:28:23.880948Z`.
pub(crate) fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Whether an enumerated serial port is a candidate FC link: a USB device
/// (regardless of its device-node name) OR a name matching a known serial
/// prefix. A single gate, so a typed USB port whose name does not match a
/// prefix (a USB gadget serial node, a vendor `ttymxc*`, a by-id symlink) is
/// not silently dropped.
pub(crate) fn is_candidate_port(port_type: &SerialPortType, name: &str) -> bool {
    matches!(port_type, SerialPortType::UsbPort(_))
        || SERIAL_PREFIXES.iter().any(|pre| name.starts_with(pre))
}

/// Whether two serial-device paths name the same device node. A literal match
/// wins; otherwise both paths are canonicalized so a symlink spelling
/// (`/dev/serial/by-id/…` vs `/dev/ttyUSB0`) still compares equal. When either
/// path cannot be resolved (node absent, non-Linux host) the check degrades to
/// the literal compare — it never guesses two distinct spellings equal.
pub(crate) fn same_device(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Identify an MSP flight controller (Betaflight / iNav) from the USB descriptor
/// of an opened serial port, by reading the device's `product`/`manufacturer`
/// strings out of sysfs. Returns `Some("betaflight")` / `Some("inav")` for a
/// recognised MSP board, else `None` (a MAVLink or unknown FC). This is the
/// passive signal that a Betaflight-over-USB board is attached even though it
/// emits nothing until polled (so the byte sniff never sees it). A `udp:`/`tcp:`
/// network FC or a non-USB serial node has no descriptor and returns `None`.
#[cfg(target_os = "linux")]
pub(crate) fn fc_variant_for_port(path: &str) -> Option<String> {
    let node = std::path::Path::new(path).file_name()?.to_str()?;
    // `/sys/class/tty/<node>/device` points at the USB *interface*; the
    // product/manufacturer strings live one or more levels up on the USB device.
    let mut cur = std::fs::canonicalize(format!("/sys/class/tty/{node}/device")).ok()?;
    for _ in 0..6 {
        let product = std::fs::read_to_string(cur.join("product")).unwrap_or_default();
        let manufacturer = std::fs::read_to_string(cur.join("manufacturer")).unwrap_or_default();
        let hay = format!("{product} {manufacturer}").to_ascii_lowercase();
        if !hay.trim().is_empty() {
            // Reached the USB device level (it carries the strings).
            if hay.contains("betaflight") {
                return Some("betaflight".to_string());
            }
            if hay.contains("inav") {
                return Some("inav".to_string());
            }
            return None;
        }
        cur = cur.parent()?.to_path_buf();
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn fc_variant_for_port(_path: &str) -> Option<String> {
    None
}

/// Write the full buffer and flush it, surfacing the first io error so the
/// caller can log it. Split out from `send_bytes` so a writer can be swapped
/// for a fault-injecting one in tests.
pub(crate) async fn write_then_flush(w: &mut BoxedWriteHalf, data: &[u8]) -> std::io::Result<()> {
    w.write_all(data).await?;
    w.flush().await
}

/// Persist the serialised parameter bytes to disk off the reactor. The atomic
/// temp-file + rename write is blocking disk I/O, so it runs on a blocking pool
/// thread rather than stalling a tokio worker. Fire-and-forget: a write failure
/// is logged, not awaited, so the read loop is never delayed by the disk.
pub(crate) fn persist_params(path: std::path::PathBuf, body: Vec<u8>) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::param_cache::write_atomic(&path, &body) {
            tracing::warn!(error = %e, "param_cache_save_failed");
        }
    });
}

/// Open a serial port at the given baud as an async stream.
pub(crate) fn open_serial(port: &str, baud: u32) -> Option<SerialStream> {
    tokio_serial::new(port, baud).open_native_async().ok()
}

/// Split a serial stream into the boxed read/write halves the run loop expects.
pub(crate) fn split_serial(
    stream: SerialStream,
    port: String,
    baud: u32,
) -> (BoxedReadHalf, BoxedWriteHalf, String, u32) {
    let (rd, wr) = tokio::io::split(stream);
    (Box::pin(rd), Box::pin(wr), port, baud)
}

/// A parsed network connection target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetSpec {
    Tcp(String),
    Udp(String),
}

/// Parse a `tcp:host:port` or `udp:host:port` connection string into a
/// `host:port` address. A bare host with no port defaults to 14550 (the
/// conventional MAVLink UDP port). Returns `None` for plain serial paths.
pub(crate) fn parse_net_spec(s: &str) -> Option<NetSpec> {
    let (scheme, rest) = s.split_once(':')?;
    let rest = rest.trim_start_matches('/');
    let addr = if rest.contains(':') {
        rest.to_string()
    } else if rest.is_empty() {
        return None;
    } else {
        format!("{rest}:14550")
    };
    match scheme {
        "tcp" => Some(NetSpec::Tcp(addr)),
        // The UDP connect/bind variants (udp/udpout/udpin) all resolve to one
        // path here: the router connects its socket to the configured peer.
        "udp" | "udpout" | "udpin" => Some(NetSpec::Udp(addr)),
        _ => None,
    }
}

/// Open at `baud` and listen for an FC HEARTBEAT within [`PROBE_WINDOW`], also
/// counting MSP frame starts so a board emitting MSP (not MAVLink) is detected
/// and the sweep can stop early. Returns [`ProbeOutcome::Heartbeat`] on the first
/// decoded HEARTBEAT, [`ProbeOutcome::Msp`] once at least two MSP frame starts
/// are seen with no HEARTBEAT, and [`ProbeOutcome::None`] when the window expires
/// with neither.
pub(crate) async fn probe_baud(port: &str, baud: u32) -> ProbeOutcome {
    let Some(mut stream) = open_serial(port, baud) else {
        return ProbeOutcome::None;
    };
    let deadline = tokio::time::Instant::now() + PROBE_WINDOW;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    let mut msp_starts: usize = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return ProbeOutcome::None;
        }
        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return ProbeOutcome::None,
            Ok(Ok(n)) => {
                msp_starts += count_msp_frame_starts(&chunk[..n]);
                buf.extend_from_slice(&chunk[..n]);
                for frame in extract_frames(&mut buf) {
                    if let Ok((_, MavMessage::HEARTBEAT(_))) = mavlink::parse_any(&frame) {
                        return ProbeOutcome::Heartbeat;
                    }
                }
                // No MAVLink HEARTBEAT here but a clear MSP stream: stop probing,
                // no other baud will produce a HEARTBEAT from an MSP FC.
                if msp_starts >= 2 {
                    return ProbeOutcome::Msp;
                }
            }
            Ok(Err(_)) => return ProbeOutcome::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_net_spec_detects_tcp_and_udp() {
        assert_eq!(
            parse_net_spec("tcp:127.0.0.1:5760"),
            Some(NetSpec::Tcp("127.0.0.1:5760".to_string()))
        );
        assert_eq!(
            parse_net_spec("udp:127.0.0.1:14550"),
            Some(NetSpec::Udp("127.0.0.1:14550".to_string()))
        );
        // udpout/udpin map to the same connect path.
        assert_eq!(
            parse_net_spec("udpout:10.0.0.5:14555"),
            Some(NetSpec::Udp("10.0.0.5:14555".to_string()))
        );
        // A bare host defaults to the conventional MAVLink UDP port.
        assert_eq!(
            parse_net_spec("udp:localhost"),
            Some(NetSpec::Udp("localhost:14550".to_string()))
        );
    }

    /// A USB serial port whose device name does not match a known prefix. The
    /// candidate gate must keep it on the strength of its USB type alone so a
    /// non-standard-named FC is still auto-discovered.
    fn usb_port_type() -> SerialPortType {
        SerialPortType::UsbPort(tokio_serial::UsbPortInfo {
            vid: 0x1209,
            pid: 0x5741,
            serial_number: None,
            manufacturer: None,
            product: None,
        })
    }

    #[test]
    fn candidate_gate_keeps_typed_usb_port_with_non_prefix_name() {
        // A USB port enumerating under a name no SERIAL_PREFIX matches.
        assert!(
            is_candidate_port(&usb_port_type(), "/dev/ttyGS0"),
            "a typed USB port must survive regardless of its device name"
        );
        assert!(
            is_candidate_port(&usb_port_type(), "/dev/ttymxc3"),
            "a vendor-named USB tty must survive"
        );
    }

    #[test]
    fn candidate_gate_keeps_prefix_named_non_usb_port() {
        // A non-USB port whose name matches a prefix (e.g. an on-board UART).
        assert!(is_candidate_port(&SerialPortType::Unknown, "/dev/ttyAMA0"));
        assert!(is_candidate_port(&SerialPortType::PciPort, "/dev/ttyS0"));
    }

    #[test]
    fn candidate_gate_rejects_non_usb_unprefixed_port() {
        // Neither USB nor a known prefix: a virtual/Bluetooth port is skipped.
        assert!(!is_candidate_port(
            &SerialPortType::BluetoothPort,
            "/dev/rfcomm0"
        ));
        assert!(!is_candidate_port(
            &SerialPortType::Unknown,
            "/dev/something-else"
        ));
    }

    #[test]
    fn same_device_matches_literal_and_resolved_paths() {
        // Literal equality needs no filesystem.
        assert!(same_device("/dev/ttyUSB0", "/dev/ttyUSB0"));
        // Distinct unresolvable paths never compare equal.
        assert!(!same_device("/dev/ttyUSB0", "/dev/ttyUSB1"));
        assert!(!same_device("/dev/ttyUSB0", ""));
    }

    #[cfg(unix)]
    #[test]
    fn same_device_resolves_a_symlink_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ttyUSB0");
        std::fs::write(&real, b"").unwrap();
        let link = dir.path().join("by-id-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_device(real.to_str().unwrap(), link.to_str().unwrap()));
        // A symlink to a DIFFERENT node stays unequal.
        let other = dir.path().join("ttyUSB1");
        std::fs::write(&other, b"").unwrap();
        assert!(!same_device(
            other.to_str().unwrap(),
            link.to_str().unwrap()
        ));
    }

    #[test]
    fn parse_net_spec_rejects_serial_paths() {
        assert_eq!(parse_net_spec("/dev/ttyACM0"), None);
        assert_eq!(parse_net_spec("/dev/ttyS0"), None);
        assert_eq!(parse_net_spec(""), None);
        // An unknown scheme is not a network transport.
        assert_eq!(parse_net_spec("serial:/dev/ttyUSB0"), None);
    }
}
