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
//!
//! The module is split by concern into siblings of this orchestrator:
//! [`framing`] (MAVLink frame extraction), [`transport`] (serial discovery +
//! TCP/UDP open + the write/persist helpers), and [`send_scheduler`] (the FC
//! send paths and the three periodic send cadences). This file owns the
//! [`FcConnection`] state and the connect/read reconnect FSM.

mod framing;
mod send_scheduler;
mod transport;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::{self, MavHeader};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, Mutex};

use crate::config::MavlinkConfig;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

use framing::extract_frames;
use send_scheduler::STREAM_DEFAULT;
use transport::{
    is_candidate_port, now_iso, open_serial, parse_net_spec, persist_params, probe_baud,
    split_serial, BoxedReadHalf, BoxedWriteHalf, NetSpec, UdpAdapter, BAUD_CANDIDATES,
    BAUD_FALLBACK,
};

/// Reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Frame fan-out channel capacity (raw frames awaiting consumers).
const FRAME_CHANNEL_CAP: usize = 1024;

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

    /// Hardware-free demo loop. Instead of opening a serial link, a synthetic
    /// source ([`crate::demo`]) generates the circular-flight telemetry at 10 Hz
    /// and pushes it through the SAME paths a real FC drives: every frame is
    /// fanned out to the MAVLink socket and the GCS proxies, and every message
    /// updates the shared [`VehicleState`] so the state snapshot the service
    /// publishes is shape- and value-compatible with the Python demo's. The
    /// link reports as connected (port `demo`, baud 0) for the run's lifetime.
    /// Returns only on shutdown via `cancel`.
    pub async fn run_demo(&self, cancel: std::sync::Arc<tokio::sync::Notify>) {
        *self.port.lock().await = "demo".to_string();
        self.baud.store(0, Ordering::Relaxed);
        self.connected.store(true, Ordering::Relaxed);
        *self.last_msg_at.lock().await = Instant::now();
        tracing::info!("fc_demo_started");

        let start = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut since_save = Instant::now();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let t = start.elapsed().as_secs_f64();
                    let now = now_iso();
                    for msg in crate::demo::demo_messages(t) {
                        // Fan the frame out exactly as a received FC frame would
                        // be: serialised once with a vehicle source identity and
                        // broadcast to every consumer (drop if no consumers).
                        let header = MavHeader {
                            system_id: crate::demo::DEMO_SYSTEM_ID,
                            component_id: crate::demo::DEMO_COMPONENT_ID,
                            sequence: self.next_seq(),
                        };
                        if let Ok(bytes) = mavlink::serialize_v2(header, &msg) {
                            let _ = self.frame_tx.send(bytes);
                        }
                        // Drive the shared state through the normal aggregator.
                        let persist = {
                            let mut st = self.state.lock().await;
                            st.update_from_message(&msg, &now)
                        };
                        if let Some((name, value, ptype)) = persist {
                            // Same off-reactor persistence as the live read loop:
                            // snapshot the bytes under the lock, then write them
                            // off the reactor.
                            let snapshot = {
                                let mut pc = self.params.lock().await;
                                pc.set(&name, value as f64, ptype);
                                if since_save.elapsed() >= Duration::from_secs(2) {
                                    since_save = Instant::now();
                                    pc.serialize().ok().map(|body| (pc.path().to_path_buf(), body))
                                } else {
                                    None
                                }
                            };
                            if let Some((path, body)) = snapshot {
                                persist_params(path, body);
                            }
                        }
                    }
                    *self.last_msg_at.lock().await = Instant::now();
                }
                _ = cancel.notified() => {
                    self.connected.store(false, Ordering::Relaxed);
                    tracing::info!("fc_demo_stopped");
                    return;
                }
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
                // versions; nothing here re-encodes a received frame. This same
                // fan-out is the plugin lane: a subscribed consumer (the MAVLink
                // socket) receives every frame, including a TUNNEL one, and
                // filters for its own private payload_type on its side.
                let _ = self.frame_tx.send(frame.clone());
                *self.last_msg_at.lock().await = Instant::now();
                // Classify a TUNNEL (385) carrying a private (application)
                // payload_type. The typed parser below rejects an unregistered
                // payload_type as an unknown enum value, so this reads the type
                // straight off the wire. This is observe-only: the frame is
                // already on the plugin lane (the fan-out above); a TUNNEL is a
                // transparent opaque pipe and is NEVER decoded, acted on, or
                // re-sent toward the flight controller from here. The read loop
                // only consumes FC->host frames, so there is no FC-injection path
                // at this point regardless.
                if let Some(payload_type) = mavlink::tunnel_payload_type(&frame) {
                    if payload_type > mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
                        tracing::debug!(payload_type, "mavlink_tunnel_inbound_to_plugin_lane");
                    }
                }
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
                        // Persist periodically, not on every parameter, to bound IO.
                        // Serialise under the lock, release it, then do the disk
                        // write off-reactor so neither the params lock nor a
                        // worker thread is held across blocking I/O.
                        let snapshot = {
                            let mut pc = self.params.lock().await;
                            pc.set(&name, value as f64, ptype);
                            if since_save.elapsed() >= Duration::from_secs(2) {
                                since_save = Instant::now();
                                pc.serialize()
                                    .ok()
                                    .map(|body| (pc.path().to_path_buf(), body))
                            } else {
                                None
                            }
                        };
                        if let Some((path, body)) = snapshot {
                            persist_params(path, body);
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
                .filter(|p| is_candidate_port(&p.port_type, &p.port_name))
                .map(|p| p.port_name)
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}
