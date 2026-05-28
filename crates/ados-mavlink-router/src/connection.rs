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
//! Transport scope: real serial only (the production path on the dev rigs).
//! SITL `tcp:`/`udp:` connection strings are a separate transport follow-up.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavCmd, MavMessage, MavModeFlag, MavState, MavType, COMMAND_LONG_DATA,
    HEARTBEAT_DATA, PARAM_REQUEST_LIST_DATA,
};
use ados_protocol::mavlink::{self, MavHeader};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{broadcast, Mutex};
use tokio_serial::{SerialPortBuilderExt, SerialPortType, SerialStream};

use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

/// Serial device name prefixes scanned when no explicit port is configured.
const SERIAL_PREFIXES: &[&str] = &[
    "/dev/ttyACM",
    "/dev/ttyAMA",
    "/dev/ttyUSB",
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

/// Drain every complete MAVLink v2 frame from the head of `buf`, returning the
/// raw frame byte vectors and leaving any partial trailing frame in `buf`.
///
/// A v2 frame is `0xFD`, a 1-byte payload length `L`, the rest of the 10-byte
/// header, `L` payload bytes, a 2-byte checksum, and (when the incompat-flags
/// signed bit is set) a 13-byte signature. Junk before the magic byte is
/// dropped. Returns when the buffer holds only a partial frame.
fn extract_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    const STX: u8 = 0xFD;
    let mut out = Vec::new();
    loop {
        // Drop bytes before the next start-of-frame magic.
        match buf.iter().position(|&b| b == STX) {
            Some(0) => {}
            Some(n) => {
                buf.drain(..n);
            }
            None => {
                buf.clear();
                break;
            }
        }
        // Need the length and incompat-flags bytes to size the frame.
        if buf.len() < 3 {
            break;
        }
        let payload_len = buf[1] as usize;
        let signed = (buf[2] & 0x01) != 0;
        let total = 10 + payload_len + 2 + if signed { 13 } else { 0 };
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
    writer: Mutex<Option<WriteHalf<SerialStream>>>,
    seq: AtomicU8,
    /// FC system id learned from inbound heartbeats (default 1 = ArduPilot).
    target_system: AtomicU8,
    connected: AtomicBool,
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
            seq: AtomicU8::new(0),
            target_system: AtomicU8::new(1),
            connected: AtomicBool::new(false),
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

    /// Write raw bytes to the FC (a client command). No-op when disconnected;
    /// a write error drops the link so the run loop reconnects. Mirrors the
    /// Python `send_bytes` (best-effort, swallow on closed link).
    pub async fn send_bytes(&self, data: &[u8]) {
        let mut guard = self.writer.lock().await;
        if let Some(w) = guard.as_mut() {
            if w.write_all(data).await.is_err() || w.flush().await.is_err() {
                *guard = None;
                self.connected.store(false, Ordering::Relaxed);
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
    pub async fn run(&self, cancel: std::sync::Arc<tokio::sync::Notify>) {
        let mut backoff = RECONNECT_MIN;
        loop {
            let stream = tokio::select! {
                s = self.open() => s,
                _ = cancel.notified() => return,
            };
            let Some((stream, port, baud)) = stream else {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.notified() => return,
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
                continue;
            };
            backoff = RECONNECT_MIN;
            *self.port.lock().await = port.clone();
            self.baud.store(baud, Ordering::Relaxed);
            let (read_half, write_half) = tokio::io::split(stream);
            *self.writer.lock().await = Some(write_half);
            self.connected.store(true, Ordering::Relaxed);
            *self.last_msg_at.lock().await = Instant::now();
            tracing::info!(port = %port, baud, "fc_connected");

            tokio::select! {
                _ = self.read_loop(read_half) => {}
                _ = cancel.notified() => {
                    self.connected.store(false, Ordering::Relaxed);
                    *self.writer.lock().await = None;
                    return;
                }
            }

            // Link dropped: reset state and reconnect.
            self.connected.store(false, Ordering::Relaxed);
            *self.writer.lock().await = None;
            self.param_priming.store(false, Ordering::Relaxed);
            *self.param_sweep_started.lock().await = None;
            *self.param_last_request.lock().await = None;
            tracing::warn!("fc_disconnected");
        }
    }

    async fn read_loop(&self, mut reader: ReadHalf<SerialStream>) {
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
                // Fan the raw frame out (drop if no consumers / lagging).
                let _ = self.frame_tx.send(frame.clone());
                *self.last_msg_at.lock().await = Instant::now();
                if let Ok((header, msg)) = mavlink::parse_v2(&frame) {
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

    /// Discover (or use the configured) serial port, baud-probe it, and open it.
    /// Returns `(stream, port, baud)` on success.
    async fn open(&self) -> Option<(SerialStream, String, u32)> {
        let candidates = self.candidate_ports();
        for port in candidates {
            // A configured baud skips the probe; otherwise probe the candidates.
            if self.cfg.baud_rate != 0 && !self.cfg.serial_port.is_empty() {
                if let Some(stream) = open_serial(&port, self.cfg.baud_rate) {
                    return Some((stream, port, self.cfg.baud_rate));
                }
                continue;
            }
            for &baud in BAUD_CANDIDATES {
                if probe_baud(&port, baud).await {
                    if let Some(stream) = open_serial(&port, baud) {
                        return Some((stream, port, baud));
                    }
                }
            }
            // Last-ditch: open at the fallback baud without a positive probe.
            if let Some(stream) = open_serial(&port, BAUD_FALLBACK) {
                return Some((stream, port, BAUD_FALLBACK));
            }
        }
        None
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

/// Open a serial port at the given baud as an async stream.
fn open_serial(port: &str, baud: u32) -> Option<SerialStream> {
    tokio_serial::new(port, baud).open_native_async().ok()
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
                    if let Ok((_, MavMessage::HEARTBEAT(_))) = mavlink::parse_v2(&frame) {
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
}
