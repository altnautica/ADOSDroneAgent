//! Flight-controller serial link.
//!
//! Mirrors the Python `FCConnection` (services/mavlink/connection.py): discover
//! and baud-probe the FC serial device, hold the link with reconnect-on-drop
//! backoff, parse the inbound MAVLink v2 byte stream into frames, fan every raw
//! frame out to consumers (the MAVLink socket + the GCS proxies), feed the
//! parsed message into the shared [`VehicleState`], and own the send paths
//! toward the FC (client commands, the 1 Hz companion heartbeat, the adaptive
//! stream-interval requests, and the parameter sweep).
//!
//! Transport scope: serial is the default and the production path on the dev
//! rigs. A configured port starting with `tcp:` or `udp:` opens a TCP or UDP
//! MAVLink transport instead, for SITL bench and demo use.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavCmd, MavMessage, MavModeFlag, MavState, MavType, COMMAND_LONG_DATA,
    HEARTBEAT_DATA, PARAM_REQUEST_LIST_DATA,
};
use ados_protocol::mavlink::{self, MavHeader};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, Mutex};
use tokio_serial::{SerialPortBuilderExt, SerialPortType, SerialStream};

use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

/// A duplex MAVLink byte transport: serial, TCP, or (datagram) UDP. Reading and
/// writing go through the boxed [`AsyncRead`]/[`AsyncWrite`] halves below, so the
/// run loop is transport-agnostic. Serial is the default on the dev rigs; the
/// `tcp:`/`udp:` connection strings exist for SITL.
type BoxedReadHalf = Pin<Box<dyn AsyncRead + Send + Unpin>>;
type BoxedWriteHalf = Pin<Box<dyn AsyncWrite + Send + Unpin>>;

/// Wraps a connected [`UdpSocket`] as a duplex byte stream. MAVLink datagrams
/// arrive one frame per packet, so a read yields one datagram's bytes and a
/// write sends one datagram. The socket is connected to the peer first so plain
/// `send`/`recv` reach it.
struct UdpAdapter {
    sock: std::sync::Arc<UdpSocket>,
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

/// Baud rates probed in order; falls back to the last one.
const BAUD_CANDIDATES: &[u32] = &[921600, 115200, 57600];
const BAUD_FALLBACK: u32 = 57600;

/// How long to listen for an FC heartbeat at each candidate baud.
const PROBE_WINDOW: Duration = Duration::from_secs(3);

/// Reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Adaptive stream-request cadence (mirrors connection.py:24-32).
const STREAM_MIN: Duration = Duration::from_secs(10);
const STREAM_DEFAULT: Duration = Duration::from_secs(30);
const STREAM_MAX: Duration = Duration::from_secs(60);
const STREAM_STALL: Duration = Duration::from_secs(5);
const STREAM_HEALTHY: Duration = Duration::from_secs(2);
const STREAM_STEP: Duration = Duration::from_secs(5);

/// Per-message stream rates requested from the FC: `(MAVLink message id, Hz)`.
const STREAM_RATES: &[(u32, f32)] = &[
    (0, 1.0),   // HEARTBEAT
    (30, 10.0), // ATTITUDE
    (33, 5.0),  // GLOBAL_POSITION_INT
    (1, 2.0),   // SYS_STATUS
    (24, 2.0),  // GPS_RAW_INT
    (74, 4.0),  // VFR_HUD
    (147, 1.0), // BATTERY_STATUS
    (65, 4.0),  // RC_CHANNELS
];

/// Parameter sweep timing.
const PARAM_RATE_LIMIT: Duration = Duration::from_secs(10);
const PARAM_DEADLINE: Duration = Duration::from_secs(30);

/// Frame fan-out channel capacity (raw frames awaiting consumers).
const FRAME_CHANNEL_CAP: usize = 1024;

/// Current ISO-8601 UTC timestamp, e.g. `2026-05-28T15:28:23.880948Z`.
fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Both MAVLink start-of-frame magic bytes: `0xFD` (v2) and `0xFE` (v1).
const STX_V2: u8 = 0xFD;
const STX_V1: u8 = 0xFE;

/// Total byte length of a complete frame whose head is at `buf[0]`, or `None`
/// when more bytes are needed (or the head is not a recognised magic byte).
///
/// A v2 frame is `0xFD`, a 1-byte payload length `L`, the rest of the 10-byte
/// header, `L` payload bytes, a 2-byte checksum, and (when the incompat-flags
/// signed bit is set) a 13-byte signature. A v1 frame is `0xFE`, a 1-byte
/// payload length `L`, a 6-byte header total, `L` payload bytes, and a 2-byte
/// checksum (no incompat/compat flags, no signature).
fn frame_total_len(buf: &[u8]) -> Option<usize> {
    match buf.first().copied() {
        Some(STX_V2) => {
            // Need the length and incompat-flags bytes to size a v2 frame.
            if buf.len() < 3 {
                return None;
            }
            let payload_len = buf[1] as usize;
            let signed = (buf[2] & 0x01) != 0;
            Some(10 + payload_len + 2 + if signed { 13 } else { 0 })
        }
        Some(STX_V1) => {
            // Need the length byte to size a v1 frame.
            if buf.len() < 2 {
                return None;
            }
            let payload_len = buf[1] as usize;
            Some(6 + payload_len + 2)
        }
        _ => None,
    }
}

/// Drain every complete MAVLink frame (v1 `0xFE` and v2 `0xFD`) from the head of
/// `buf`, returning the raw frame byte vectors and leaving any partial trailing
/// frame in `buf`. Junk before the next magic byte is dropped. Returns when the
/// buffer holds only a partial frame.
fn extract_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        // Drop bytes before the next start-of-frame magic (either version).
        match buf.iter().position(|&b| b == STX_V2 || b == STX_V1) {
            Some(0) => {}
            Some(n) => {
                buf.drain(..n);
            }
            None => {
                buf.clear();
                break;
            }
        }
        let Some(total) = frame_total_len(buf) else {
            // Either too few bytes to size the frame yet, or the head byte is
            // not a magic byte (cannot happen after the search above). Wait for
            // more bytes.
            break;
        };
        if buf.len() < total {
            break;
        }
        out.push(buf[..total].to_vec());
        buf.drain(..total);
    }
    out
}

/// The FC serial link plus its shared state. Cheap to wrap in an `Arc`; every
/// method takes `&self` and uses interior mutability so the run loop and the
/// periodic sender tasks share one connection.
pub struct FcConnection {
    cfg: MavlinkConfig,
    state: std::sync::Arc<Mutex<VehicleState>>,
    params: std::sync::Arc<Mutex<ParamCache>>,
    frame_tx: broadcast::Sender<Vec<u8>>,
    writer: Mutex<Option<BoxedWriteHalf>>,
    /// Raised by a write/flush failure to ask the run loop to tear down the
    /// current link and re-open it (installing a fresh writer). A transient FC
    /// write error must not permanently declare the FC disconnected, so the run
    /// loop owns recovery rather than latching the writer to `None`.
    reconnect: tokio::sync::Notify,
    seq: AtomicU8,
    /// FC system id learned from inbound heartbeats (default 1 = ArduPilot).
    target_system: AtomicU8,
    connected: AtomicBool,
    /// True once a write to the FC has succeeded since the current link opened.
    /// Drives the reconnect backoff: a session that proved writable resets to
    /// the minimum, while a port that opens but never accepts a write backs off.
    wrote_since_open: AtomicBool,
    port: Mutex<String>,
    baud: AtomicU32,
    last_msg_at: Mutex<Instant>,
    stream_interval: Mutex<Duration>,
    last_stream_req: Mutex<Option<Instant>>,
    param_priming: AtomicBool,
    param_sweep_timed_out: AtomicBool,
    param_sweep_send_failed: AtomicBool,
    param_last_request: Mutex<Option<Instant>>,
    param_sweep_started: Mutex<Option<Instant>>,
}

impl FcConnection {
    /// Build a connection sharing the given state + param cache.
    pub fn new(
        cfg: MavlinkConfig,
        state: std::sync::Arc<Mutex<VehicleState>>,
        params: std::sync::Arc<Mutex<ParamCache>>,
    ) -> std::sync::Arc<Self> {
        let (frame_tx, _) = broadcast::channel(FRAME_CHANNEL_CAP);
        std::sync::Arc::new(Self {
            cfg,
            state,
            params,
            frame_tx,
            writer: Mutex::new(None),
            reconnect: tokio::sync::Notify::new(),
            seq: AtomicU8::new(0),
            target_system: AtomicU8::new(1),
            connected: AtomicBool::new(false),
            wrote_since_open: AtomicBool::new(false),
            port: Mutex::new(String::new()),
            baud: AtomicU32::new(0),
            last_msg_at: Mutex::new(Instant::now()),
            stream_interval: Mutex::new(STREAM_DEFAULT),
            last_stream_req: Mutex::new(None),
            param_priming: AtomicBool::new(false),
            param_sweep_timed_out: AtomicBool::new(false),
            param_sweep_send_failed: AtomicBool::new(false),
            param_last_request: Mutex::new(None),
            param_sweep_started: Mutex::new(None),
        })
    }

    /// Subscribe to the raw inbound frame stream (the MAVLink socket + proxies).
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.frame_tx.subscribe()
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    pub async fn port(&self) -> String {
        self.port.lock().await.clone()
    }
    pub fn baud(&self) -> u32 {
        self.baud.load(Ordering::Relaxed)
    }
    pub fn param_priming(&self) -> bool {
        self.param_priming.load(Ordering::Relaxed)
    }
    pub fn param_sweep_timed_out(&self) -> bool {
        self.param_sweep_timed_out.load(Ordering::Relaxed)
    }
    pub fn param_sweep_send_failed(&self) -> bool {
        self.param_sweep_send_failed.load(Ordering::Relaxed)
    }

    fn next_seq(&self) -> u8 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn our_header(&self) -> MavHeader {
        MavHeader {
            system_id: self.cfg.system_id,
            component_id: self.cfg.component_id,
            sequence: self.next_seq(),
        }
    }

    /// Write raw bytes to the FC (a client command). No-op when disconnected.
    /// On a write/flush error the current writer is dropped and a reconnect is
    /// signalled so the run loop tears the link down and re-opens it with a
    /// fresh writer. The write path deliberately does NOT clear `connected`:
    /// the run loop owns that lifecycle, so a transient write error during a
    /// heavy parameter dump (with reads still flowing) recovers to a live link
    /// rather than latching the FC permanently "disconnected".
    pub async fn send_bytes(&self, data: &[u8]) {
        let mut guard = self.writer.lock().await;
        if let Some(w) = guard.as_mut() {
            match write_then_flush(w, data).await {
                Ok(()) => self.wrote_since_open.store(true, Ordering::Relaxed),
                Err(e) => {
                    *guard = None;
                    drop(guard);
                    tracing::warn!(error = %e, "fc_write_failed");
                    self.reconnect.notify_one();
                }
            }
        }
    }

    async fn send_msg(&self, msg: &MavMessage) -> bool {
        match mavlink::serialize_v2(self.our_header(), msg) {
            Ok(bytes) => {
                self.send_bytes(&bytes).await;
                true
            }
            Err(_) => false,
        }
    }

    /// Send the 1 Hz companion heartbeat so the FC registers a valid GCS-class
    /// component and does not trip its GCS failsafe.
    pub async fn send_heartbeat(&self) {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_UNINIT,
            mavlink_version: 3,
        });
        self.send_msg(&msg).await;
    }

    /// Adaptive stream request. Picks the interval from how long the link has
    /// been idle (stalled link re-requests fast; healthy link relaxes toward
    /// the max), then re-sends the per-message rates when the interval elapses.
    pub async fn tick_streams(&self) {
        if !self.connected() {
            return;
        }
        let idle = self.last_msg_at.lock().await.elapsed();
        {
            let mut interval = self.stream_interval.lock().await;
            *interval = if idle >= STREAM_STALL {
                STREAM_MIN
            } else if idle <= STREAM_HEALTHY {
                (*interval + STREAM_STEP).min(STREAM_MAX)
            } else {
                *interval
            };
        }
        let interval = *self.stream_interval.lock().await;
        let due = {
            let last = self.last_stream_req.lock().await;
            last.map(|t| t.elapsed() >= interval).unwrap_or(true)
        };
        if !due {
            return;
        }
        let target = self.target_system.load(Ordering::Relaxed);
        for &(msg_id, rate_hz) in STREAM_RATES {
            let interval_us = 1_000_000.0 / rate_hz;
            let cmd = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                target_system: target,
                target_component: 1,
                command: MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL,
                confirmation: 0,
                param1: msg_id as f32,
                param2: interval_us,
                param3: 0.0,
                param4: 0.0,
                param5: 0.0,
                param6: 0.0,
                param7: 0.0,
            });
            self.send_msg(&cmd).await;
        }
        *self.last_stream_req.lock().await = Some(Instant::now());
    }

    /// Parameter sweep with the priming/timeout flags the GCS spinner reads.
    /// Rate-limited to one PARAM_REQUEST_LIST per [`PARAM_RATE_LIMIT`]; flips
    /// the timeout flag when the deadline passes with no parameters cached.
    pub async fn tick_param_sweep(&self) {
        if !self.connected() {
            return;
        }
        // Progress check: clear priming once the cache is fully populated.
        let cached = self.params.lock().await.count();
        let expected = self.state.lock().await.param_count.max(0) as usize;
        if expected > 0 && cached >= expected {
            self.param_priming.store(false, Ordering::Relaxed);
            self.param_sweep_timed_out.store(false, Ordering::Relaxed);
            return;
        }
        let due = {
            let last = self.param_last_request.lock().await;
            last.map(|t| t.elapsed() >= PARAM_RATE_LIMIT)
                .unwrap_or(true)
        };
        if !due {
            // While priming and past the deadline with nothing cached, flag the timeout.
            if self.param_priming.load(Ordering::Relaxed) && cached == 0 {
                if let Some(started) = *self.param_sweep_started.lock().await {
                    if started.elapsed() >= PARAM_DEADLINE {
                        self.param_sweep_timed_out.store(true, Ordering::Relaxed);
                    }
                }
            }
            return;
        }
        let target = self.target_system.load(Ordering::Relaxed);
        let req = MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
            target_system: target,
            target_component: 1,
        });
        let ok = self.send_msg(&req).await;
        self.param_sweep_send_failed.store(!ok, Ordering::Relaxed);
        self.param_priming.store(true, Ordering::Relaxed);
        let now = Instant::now();
        *self.param_last_request.lock().await = Some(now);
        let mut started = self.param_sweep_started.lock().await;
        if started.is_none() {
            *started = Some(now);
        }
    }

    /// Connect-and-read loop. Returns only on shutdown via `cancel`.
    ///
    /// Three things end a live session: the read half hits EOF/error (the FC
    /// went away), `cancel` fires (shutdown), or a write failure raises the
    /// reconnect signal (a transient agent->FC write error). All three fall
    /// through to one teardown + re-open path so the writer is always replaced
    /// with a fresh half. A persistently unwritable port is kept from
    /// tight-looping by a bounded backoff that only grows while the link never
    /// proves healthy, and resets the moment a session holds long enough to be
    /// considered up.
    pub async fn run(&self, cancel: std::sync::Arc<tokio::sync::Notify>) {
        let mut backoff = RECONNECT_MIN;
        loop {
            let stream = tokio::select! {
                s = self.open() => s,
                _ = cancel.notified() => return,
            };
            let Some((read_half, write_half, port, baud)) = stream else {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.notified() => return,
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
                continue;
            };
            *self.port.lock().await = port.clone();
            self.baud.store(baud, Ordering::Relaxed);
            *self.writer.lock().await = Some(write_half);
            self.connected.store(true, Ordering::Relaxed);
            self.wrote_since_open.store(false, Ordering::Relaxed);
            *self.last_msg_at.lock().await = Instant::now();
            tracing::info!(port = %port, baud, "fc_connected");

            tokio::select! {
                _ = self.read_loop(read_half) => {}
                // A write failure asks the loop to rebuild the link. Tear down
                // the read half and fall through to the re-open below, which
                // installs a fresh writer; do not declare the FC disconnected
                // permanently for what may be a transient write error.
                _ = self.reconnect.notified() => {
                    tracing::warn!("fc_write_failed_reconnecting");
                }
                _ = cancel.notified() => {
                    self.connected.store(false, Ordering::Relaxed);
                    *self.writer.lock().await = None;
                    return;
                }
            }

            // Link dropped (read EOF/error or a write failure): reset state and
            // reconnect.
            self.connected.store(false, Ordering::Relaxed);
            *self.writer.lock().await = None;
            self.param_priming.store(false, Ordering::Relaxed);
            *self.param_sweep_started.lock().await = None;
            *self.param_last_request.lock().await = None;
            tracing::warn!("fc_disconnected");

            // A session that proved writable (at least one write to the FC
            // succeeded) is healthy: reset and re-open immediately so the common
            // transient case recovers fast. A port that opens but never accepts
            // a write (readable-but-unwritable) never sets this, so it backs off
            // instead of re-opening on every failed write — including the 1 Hz
            // companion heartbeat, which would otherwise pin the loop at ~1 Hz.
            if self.wrote_since_open.load(Ordering::Relaxed) {
                backoff = RECONNECT_MIN;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.notified() => return,
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }

    async fn read_loop(&self, mut reader: BoxedReadHalf) {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        let mut since_save = Instant::now();
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) => return, // EOF: link gone
                Ok(n) => n,
                Err(_) => return,
            };
            buf.extend_from_slice(&chunk[..n]);
            // Cap the reassembly buffer so a stream of junk cannot grow it.
            if buf.len() > 1 << 20 {
                buf.clear();
            }
            for frame in extract_frames(&mut buf) {
                // Fan the raw frame out verbatim (drop if no consumers / lagging).
                // The original bytes are forwarded unchanged for both protocol
                // versions; nothing here re-encodes a received frame.
                let _ = self.frame_tx.send(frame.clone());
                *self.last_msg_at.lock().await = Instant::now();
                if let Ok((header, msg)) = mavlink::parse_any(&frame) {
                    // Learn the FC system id from its (non-GCS) heartbeats.
                    if let MavMessage::HEARTBEAT(_) = &msg {
                        if header.system_id != self.cfg.system_id {
                            self.target_system
                                .store(header.system_id, Ordering::Relaxed);
                        }
                    }
                    let now = now_iso();
                    let persist = {
                        let mut st = self.state.lock().await;
                        st.update_from_message(&msg, &now)
                    };
                    if let Some((name, value, ptype)) = persist {
                        let mut pc = self.params.lock().await;
                        pc.set(&name, value as f64, ptype);
                        // Persist periodically, not on every parameter, to bound IO.
                        if since_save.elapsed() >= Duration::from_secs(2) {
                            let _ = pc.save();
                            since_save = Instant::now();
                        }
                    }
                }
            }
        }
    }

    /// Open the configured (or discovered) transport. A configured port that
    /// starts with `tcp:` or `udp:` opens a network MAVLink transport; otherwise
    /// the serial discovery + baud-probe path runs. Returns the read/write halves
    /// plus the port label and (serial only) baud on success.
    async fn open(&self) -> Option<(BoxedReadHalf, BoxedWriteHalf, String, u32)> {
        // SITL / network transport: detected from the configured connection
        // string, never from serial discovery (baud is not meaningful here).
        let configured = self.cfg.serial_port.trim();
        if let Some(spec) = parse_net_spec(configured) {
            return self.open_net(spec).await;
        }

        let candidates = self.candidate_ports();
        for port in candidates {
            // A configured baud skips the probe; otherwise probe the candidates.
            if self.cfg.baud_rate != 0 && !self.cfg.serial_port.is_empty() {
                if let Some(stream) = open_serial(&port, self.cfg.baud_rate) {
                    return Some(split_serial(stream, port, self.cfg.baud_rate));
                }
                continue;
            }
            for &baud in BAUD_CANDIDATES {
                if probe_baud(&port, baud).await {
                    if let Some(stream) = open_serial(&port, baud) {
                        return Some(split_serial(stream, port, baud));
                    }
                }
            }
            // Last-ditch: open at the fallback baud without a positive probe.
            if let Some(stream) = open_serial(&port, BAUD_FALLBACK) {
                return Some(split_serial(stream, port, BAUD_FALLBACK));
            }
        }
        None
    }

    /// Open a TCP or UDP MAVLink transport for SITL. TCP connects to the peer;
    /// UDP binds a local socket and connects it to the peer so plain send/recv
    /// reach it. Baud is reported as 0 for network transports.
    async fn open_net(
        &self,
        spec: NetSpec,
    ) -> Option<(BoxedReadHalf, BoxedWriteHalf, String, u32)> {
        match spec {
            NetSpec::Tcp(addr) => match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let (rd, wr) = tokio::io::split(stream);
                    let label = format!("tcp:{addr}");
                    tracing::info!(addr = %addr, "fc_tcp_connecting");
                    Some((Box::pin(rd), Box::pin(wr), label, 0))
                }
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %e, "fc_tcp_connect_failed");
                    None
                }
            },
            NetSpec::Udp(addr) => {
                let sock = match UdpSocket::bind(("0.0.0.0", 0)).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "fc_udp_bind_failed");
                        return None;
                    }
                };
                if let Err(e) = sock.connect(&addr).await {
                    tracing::warn!(addr = %addr, error = %e, "fc_udp_connect_failed");
                    return None;
                }
                let sock = std::sync::Arc::new(sock);
                let (rd, wr) = tokio::io::split(UdpAdapter { sock: sock.clone() });
                let label = format!("udp:{addr}");
                tracing::info!(addr = %addr, "fc_udp_bound");
                Some((Box::pin(rd), Box::pin(wr), label, 0))
            }
        }
    }

    fn candidate_ports(&self) -> Vec<String> {
        if !self.cfg.serial_port.is_empty() {
            return vec![self.cfg.serial_port.clone()];
        }
        match tokio_serial::available_ports() {
            Ok(ports) => ports
                .into_iter()
                .filter(|p| {
                    // Prefer real USB/UART devices over virtual ports.
                    matches!(p.port_type, SerialPortType::UsbPort(_))
                        || SERIAL_PREFIXES
                            .iter()
                            .any(|pre| p.port_name.starts_with(pre))
                })
                .map(|p| p.port_name)
                .filter(|name| SERIAL_PREFIXES.iter().any(|pre| name.starts_with(pre)))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Write the full buffer and flush it, surfacing the first io error so the
/// caller can log it. Split out from `send_bytes` so a writer can be swapped
/// for a fault-injecting one in tests.
async fn write_then_flush(w: &mut BoxedWriteHalf, data: &[u8]) -> std::io::Result<()> {
    w.write_all(data).await?;
    w.flush().await
}

/// Open a serial port at the given baud as an async stream.
fn open_serial(port: &str, baud: u32) -> Option<SerialStream> {
    tokio_serial::new(port, baud).open_native_async().ok()
}

/// Split a serial stream into the boxed read/write halves the run loop expects.
fn split_serial(
    stream: SerialStream,
    port: String,
    baud: u32,
) -> (BoxedReadHalf, BoxedWriteHalf, String, u32) {
    let (rd, wr) = tokio::io::split(stream);
    (Box::pin(rd), Box::pin(wr), port, baud)
}

/// A parsed network connection target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NetSpec {
    Tcp(String),
    Udp(String),
}

/// Parse a `tcp:host:port` or `udp:host:port` connection string into a
/// `host:port` address. A bare host with no port defaults to 14550 (the
/// conventional MAVLink UDP port). Returns `None` for plain serial paths.
fn parse_net_spec(s: &str) -> Option<NetSpec> {
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

/// Open at `baud` and listen for an FC HEARTBEAT within [`PROBE_WINDOW`].
async fn probe_baud(port: &str, baud: u32) -> bool {
    let Some(mut stream) = open_serial(port, baud) else {
        return false;
    };
    let deadline = tokio::time::Instant::now() + PROBE_WINDOW;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return false,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                for frame in extract_frames(&mut buf) {
                    if let Ok((_, MavMessage::HEARTBEAT(_))) = mavlink::parse_any(&frame) {
                        return true;
                    }
                }
            }
            Ok(Err(_)) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::HEARTBEAT_DATA;
    use ados_protocol::mavlink::MavHeader;

    fn heartbeat_frame() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        mavlink::serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    #[test]
    fn extract_one_complete_frame() {
        let frame = heartbeat_frame();
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_is_retained() {
        let frame = heartbeat_frame();
        let split = frame.len() - 2;
        let mut buf = frame[..split].to_vec();
        let frames = extract_frames(&mut buf);
        assert!(frames.is_empty());
        assert_eq!(buf.len(), split); // kept for the next read
                                      // Deliver the rest; now it parses.
        buf.extend_from_slice(&frame[split..]);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn junk_before_magic_is_dropped_and_two_frames_extracted() {
        let frame = heartbeat_frame();
        let mut buf = vec![0x11, 0x22, 0x33];
        buf.extend_from_slice(&frame);
        buf.extend_from_slice(&frame);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frame);
        assert_eq!(frames[1], frame);
    }

    #[test]
    fn parsed_extracted_frame_is_a_heartbeat() {
        let frame = heartbeat_frame();
        let (_h, msg) = mavlink::parse_v2(&frame).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    fn heartbeat_frame_v1() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        mavlink::serialize_v1(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    #[test]
    fn extract_one_v1_frame() {
        let frame = heartbeat_frame_v1();
        assert_eq!(frame[0], 0xFE);
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

    #[test]
    fn extracted_v1_frame_decodes_and_round_trips_bytes() {
        let frame = heartbeat_frame_v1();
        // The framer returns the exact bytes (re-broadcast verbatim, no re-encode).
        let mut buf = frame.clone();
        let frames = extract_frames(&mut buf);
        assert_eq!(frames[0], frame);
        // The decode path recognises it as a v1 heartbeat.
        let (_h, msg) = mavlink::parse_any(&frames[0]).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn extract_mixed_v1_and_v2_frames_in_one_buffer() {
        let v2 = heartbeat_frame();
        let v1 = heartbeat_frame_v1();
        let mut buf = Vec::new();
        buf.extend_from_slice(&v2);
        buf.extend_from_slice(&v1);
        buf.extend_from_slice(&v2);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], v2);
        assert_eq!(frames[1], v1);
        assert_eq!(frames[2], v2);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_v1_frame_is_retained() {
        let frame = heartbeat_frame_v1();
        let split = frame.len() - 2;
        let mut buf = frame[..split].to_vec();
        assert!(extract_frames(&mut buf).is_empty());
        buf.extend_from_slice(&frame[split..]);
        let frames = extract_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], frame);
        assert!(buf.is_empty());
    }

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

    #[test]
    fn parse_net_spec_rejects_serial_paths() {
        assert_eq!(parse_net_spec("/dev/ttyACM0"), None);
        assert_eq!(parse_net_spec("/dev/ttyS0"), None);
        assert_eq!(parse_net_spec(""), None);
        // An unknown scheme is not a network transport.
        assert_eq!(parse_net_spec("serial:/dev/ttyUSB0"), None);
    }

    /// A write half whose first `write_all` fails, standing in for a serial
    /// port that drops writes while reads keep flowing (the failure mode that
    /// used to latch the FC "disconnected" forever).
    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected write failure",
            )))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_connection() -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new("/tmp/ados-test-params.json")));
        FcConnection::new(MavlinkConfig::default(), state, params)
    }

    #[tokio::test]
    async fn write_failure_does_not_latch_disconnected_and_signals_reconnect() {
        let conn = test_connection();

        // Simulate a live link with a writer that will fail on the next write.
        conn.connected.store(true, Ordering::Relaxed);
        *conn.writer.lock().await = Some(Box::pin(FailingWriter));

        conn.send_bytes(b"\xfd\x00").await;

        // The failing writer is dropped so the run loop reinstalls a fresh one.
        assert!(
            conn.writer.lock().await.is_none(),
            "writer must be cleared after a write failure"
        );

        // The write path must NOT declare the FC permanently disconnected; the
        // run loop owns `connected` and clears it only on a real teardown.
        assert!(
            conn.connected(),
            "send_bytes must not latch connected=false on a transient write error"
        );

        // The reconnect signal must have been raised so run() rebuilds the link.
        // notify_one() leaves a permit, so notified() resolves immediately.
        let signalled = tokio::time::timeout(Duration::from_millis(100), conn.reconnect.notified())
            .await
            .is_ok();
        assert!(signalled, "a write failure must signal a reconnect");
    }

    #[tokio::test]
    async fn send_bytes_is_a_noop_when_no_writer() {
        let conn = test_connection();
        // No writer installed: send_bytes does nothing and raises no reconnect.
        conn.send_bytes(b"\xfd\x00").await;
        let signalled = tokio::time::timeout(Duration::from_millis(50), conn.reconnect.notified())
            .await
            .is_ok();
        assert!(
            !signalled,
            "no writer means nothing to fail and no reconnect to raise"
        );
    }
}
