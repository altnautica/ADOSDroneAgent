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

use framing::{count_msp_frame_starts, extract_frames};
use send_scheduler::STREAM_DEFAULT;
use transport::{
    is_candidate_port, now_iso, open_serial, parse_net_spec, persist_params, probe_baud,
    split_serial, BoxedReadHalf, BoxedWriteHalf, NetSpec, ProbeOutcome, UdpAdapter,
    BAUD_CANDIDATES, BAUD_FALLBACK,
};

/// Reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// A decoded HEARTBEAT older than this means the FC link is open but the
/// autopilot is no longer talking: the transport may still be up (the serial
/// node is open, bytes may even be flowing) but no fresh HEARTBEAT has arrived,
/// so the link is NOT confirmed alive. A port that opens at a fixed baud but
/// never sees a HEARTBEAT (a wrong baud, an unpowered FC, a cable on the wrong
/// pins) reads as transport-open-but-not-alive rather than connected. The
/// window is generous relative to ArduPilot's 1 Hz HEARTBEAT so a single
/// dropped frame never flips the state. "Presence is not proof": an open
/// transport is necessary but not sufficient to declare the FC connected.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a sighting of MSP traffic keeps the `msp_detected` link hint armed.
/// Generous relative to the read cadence so a steady MSP stream holds the hint,
/// and the hint ages out on its own once the byte stream goes quiet.
const MSP_HINT_TTL: Duration = Duration::from_secs(5);

/// The rolling window over which MSP frame starts are accumulated before the
/// link hint is armed. Requiring two starts inside this window (rather than a
/// single byte match) rejects a stray `$M`/`$X` that lands inside a MAVLink
/// payload.
const MSP_WINDOW: Duration = Duration::from_secs(2);

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
    /// Monotonic clock of the last decoded HEARTBEAT, distinct from
    /// `last_msg_at` (which bumps on ANY inbound frame bytes, including garbage
    /// that happens to form a frame). `None` until the first HEARTBEAT decodes
    /// on the current process. Drives the heartbeat-freshness gate that decides
    /// whether the link is alive, not merely transport-open. Reset to `None` on
    /// link teardown so a stale value from a prior session never reads as alive.
    last_heartbeat_at: Mutex<Option<Instant>>,
    /// Monotonic clock of the last time MSP frame starts were seen on the inbound
    /// byte stream while no HEARTBEAT was decoding. Drives the `msp_detected`
    /// link hint: an FC whose serial port is configured for MSP rather than
    /// MAVLink emits these, so the agent can tell the operator the link speaks
    /// the wrong protocol. `None` until MSP traffic is seen; cleared on a real
    /// HEARTBEAT and on link teardown so it never carries across sessions.
    last_msp_at: Mutex<Option<Instant>>,
    /// True once the MSP-detected condition has been logged for the current MSP
    /// episode, so the warning fires once per episode rather than every read.
    msp_warned: AtomicBool,
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
            last_heartbeat_at: Mutex::new(None),
            last_msp_at: Mutex::new(None),
            msp_warned: AtomicBool::new(false),
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

    /// Whether the FC transport is open: the serial node / network socket has
    /// been opened and not yet torn down. This is NOT proof the FC is talking —
    /// see [`Self::mavlink_alive`]. Kept distinct so a consumer can render
    /// "port open, no MAVLink" separately from "no port".
    pub fn transport_open(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    /// Seconds since the last decoded HEARTBEAT, or `None` when none has been
    /// seen on the current process. The freshness signal the alive gate reads.
    pub async fn heartbeat_age_s(&self) -> Option<f64> {
        self.last_heartbeat_at
            .lock()
            .await
            .map(|t| t.elapsed().as_secs_f64())
    }
    /// Whether a fresh HEARTBEAT has decoded within [`HEARTBEAT_TIMEOUT`]. The
    /// demo loop has no real HEARTBEAT clock but drives synthetic telemetry, so
    /// it reports alive whenever the transport is open (the port label `demo`).
    pub async fn mavlink_alive(&self) -> bool {
        if &*self.port.lock().await == "demo" {
            return self.connected.load(Ordering::Relaxed);
        }
        match *self.last_heartbeat_at.lock().await {
            Some(t) => t.elapsed() < HEARTBEAT_TIMEOUT,
            None => false,
        }
    }
    /// The gated truth: the FC is connected only when the transport is open AND
    /// a fresh HEARTBEAT has been decoded. "Presence is not proof" — an open
    /// serial port at a configured baud is not enough; the autopilot must be
    /// talking. Replaces the old transport-open-only `connected()` for the
    /// published `fc_connected` extra.
    pub async fn connected(&self) -> bool {
        self.transport_open() && self.mavlink_alive().await
    }
    /// A human-actionable hint about why the FC link is not alive, computed from
    /// the current liveness plus recent MSP evidence:
    ///
    ///   - `none` — the link is alive (or a demo run, or there is no transport),
    ///     so there is nothing to explain.
    ///   - `msp_detected` — the transport is open, no HEARTBEAT decoded, and MSP
    ///     frame starts were seen on the byte stream within the recent window:
    ///     the FC is configured for MSP, not MAVLink, on this port. The operator
    ///     fix is to set the FC's serial-port protocol to MAVLink.
    ///   - `no_heartbeat` — the transport is open, no HEARTBEAT decoded, and no
    ///     MSP traffic was seen: a wrong baud, an unpowered / not-yet-booted FC,
    ///     wrong wiring, or a serial protocol other than MAVLink/MSP.
    ///
    /// Derived (not latched) so it self-corrects: the moment a HEARTBEAT decodes
    /// it reads `none`, and the MSP evidence ages out via [`MSP_HINT_TTL`].
    pub async fn link_hint(&self) -> &'static str {
        if &*self.port.lock().await == "demo" {
            return "none";
        }
        if self.mavlink_alive().await {
            return "none";
        }
        if !self.transport_open() {
            return "none";
        }
        let msp_recent = self
            .last_msp_at
            .lock()
            .await
            .map(|t| t.elapsed() < MSP_HINT_TTL)
            .unwrap_or(false);
        if msp_recent {
            "msp_detected"
        } else {
            "no_heartbeat"
        }
    }
    pub async fn port(&self) -> String {
        self.port.lock().await.clone()
    }
    pub fn baud(&self) -> u32 {
        self.baud.load(Ordering::Relaxed)
    }
    /// The configured FC transport class for the snapshot: `serial`, `udp`,
    /// `tcp`, or `auto` (empty `serial_port` → discovery). Derived from the
    /// config's `source` field, falling back to the shape of `serial_port` for a
    /// config that predates the explicit field (a `udp:`/`tcp:` prefix is a
    /// network transport; a non-empty path is serial; empty is auto-detect).
    pub fn source(&self) -> &'static str {
        match self.cfg.source.trim().to_ascii_lowercase().as_str() {
            "serial" => "serial",
            "udp" => "udp",
            "tcp" => "tcp",
            "auto" => "auto",
            // Unset / unknown: infer from the connection string shape.
            _ => {
                let port = self.cfg.serial_port.trim();
                if port.is_empty() {
                    "auto"
                } else if port.starts_with("udp:") {
                    "udp"
                } else if port.starts_with("tcp:") {
                    "tcp"
                } else {
                    "serial"
                }
            }
        }
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

    /// Whether a HEARTBEAT with this source identity is our OWN injected
    /// companion heartbeat — identified by the FULL (system_id, component_id)
    /// pair, never system_id alone. A companion computer shares the vehicle's
    /// system_id (commonly 1) and is distinguished only by its component_id
    /// (191 vs the autopilot's 1), so a system_id-only check would wrongly
    /// treat the FC's own heartbeat as ours whenever the agent's configured
    /// system_id equals the FC's — and the link would never read alive even
    /// while the autopilot streams HEARTBEAT at 1 Hz.
    fn is_own_heartbeat(&self, system_id: u8, component_id: u8) -> bool {
        system_id == self.cfg.system_id && component_id == self.cfg.component_id
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
                    *self.last_heartbeat_at.lock().await = None;
                    *self.writer.lock().await = None;
                    return;
                }
            }

            // Link dropped (read EOF/error or a write failure): reset state and
            // reconnect. Clearing the heartbeat clock means the freshly re-opened
            // link reads as not-alive until a real HEARTBEAT arrives again.
            self.connected.store(false, Ordering::Relaxed);
            *self.last_heartbeat_at.lock().await = None;
            *self.last_msp_at.lock().await = None;
            self.msp_warned.store(false, Ordering::Relaxed);
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
        // Rolling MSP-evidence accumulator for the link-hint detector.
        let mut msp_count: usize = 0;
        let mut msp_window = Instant::now();
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
            // Sniff the raw chunk for MSP frame starts before reassembly drains
            // the leading junk where they live. An FC whose serial port is set to
            // MSP instead of MAVLink emits these; arming the hint lets the
            // operator be told the link speaks the wrong protocol. Observe-only:
            // MSP bytes are never decoded, acted on, or forwarded to the FC.
            let starts = count_msp_frame_starts(&chunk[..n]);
            if starts > 0 {
                if msp_window.elapsed() > MSP_WINDOW {
                    msp_count = 0;
                    msp_window = Instant::now();
                }
                msp_count += starts;
                // Two starts inside the window, and only while no HEARTBEAT is
                // live — the alive gate is what guards against a stray `$M`/`$X`
                // landing inside a MAVLink payload on a healthy link.
                if msp_count >= 2 && !self.mavlink_alive().await {
                    *self.last_msp_at.lock().await = Some(Instant::now());
                    if !self.msp_warned.swap(true, Ordering::Relaxed) {
                        let port = self.port.lock().await.clone();
                        tracing::warn!(port = %port, "fc_link_msp_detected_not_mavlink");
                    }
                }
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
                    // Learn the FC system id from its heartbeats, and stamp the
                    // heartbeat-freshness clock the alive gate reads. Bump the
                    // clock for any HEARTBEAT that is NOT our own injected
                    // companion one — identified by the full (system_id,
                    // component_id) identity, NOT system_id alone. A companion
                    // shares the vehicle's system_id (commonly 1) and differs
                    // only by component_id (191 vs the autopilot's 1), so a
                    // system_id-only filter wrongly discards the FC's own
                    // heartbeat whenever the agent's configured system_id equals
                    // the FC's — the link then never reads alive even though the
                    // autopilot is streaming. A port that opens but never hears
                    // the autopilot still stays not-alive.
                    if let MavMessage::HEARTBEAT(_) = &msg {
                        if !self.is_own_heartbeat(header.system_id, header.component_id) {
                            self.target_system
                                .store(header.system_id, Ordering::Relaxed);
                            *self.last_heartbeat_at.lock().await = Some(Instant::now());
                            // A real FC HEARTBEAT clears any MSP suspicion so a
                            // link that started noisy (or recovered) reads as
                            // healthy at once, and re-arms the once-per-episode
                            // warning for any future MSP episode.
                            *self.last_msp_at.lock().await = None;
                            self.msp_warned.store(false, Ordering::Relaxed);
                            msp_count = 0;
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
                match probe_baud(&port, baud).await {
                    ProbeOutcome::Heartbeat => {
                        if let Some(stream) = open_serial(&port, baud) {
                            return Some(split_serial(stream, port, baud));
                        }
                    }
                    ProbeOutcome::Msp => {
                        // The FC is emitting MSP, not MAVLink — no baud will yield
                        // a HEARTBEAT. Open here so the read loop surfaces the
                        // msp_detected hint, and stop sweeping the remaining bauds.
                        if let Some(stream) = open_serial(&port, baud) {
                            return Some(split_serial(stream, port, baud));
                        }
                        break;
                    }
                    ProbeOutcome::None => {}
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

#[cfg(test)]
mod liveness_tests {
    use super::*;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;

    fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-liveness-params.json",
        )));
        FcConnection::new(cfg, state, params)
    }

    #[tokio::test]
    async fn fresh_connection_is_transport_closed_and_not_alive() {
        let c = conn_with(MavlinkConfig::default());
        assert!(!c.transport_open());
        assert!(!c.mavlink_alive().await);
        assert!(!c.connected().await);
        assert!(c.heartbeat_age_s().await.is_none());
    }

    #[tokio::test]
    async fn transport_open_without_a_heartbeat_is_not_connected() {
        // The exact bug: the port opens but no HEARTBEAT decodes → the link is
        // transport-open but NOT alive, so fc_connected must be false.
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        assert!(c.transport_open());
        assert!(!c.mavlink_alive().await, "no heartbeat → not alive");
        assert!(
            !c.connected().await,
            "transport open alone is not connected"
        );
    }

    #[tokio::test]
    async fn a_fresh_heartbeat_makes_an_open_transport_connected() {
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        *c.last_heartbeat_at.lock().await = Some(Instant::now());
        assert!(c.mavlink_alive().await);
        assert!(c.connected().await);
        let age = c
            .heartbeat_age_s()
            .await
            .expect("age present after a heartbeat");
        assert!((0.0..1.0).contains(&age));
    }

    #[tokio::test]
    async fn a_stale_heartbeat_reads_not_alive() {
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        *c.last_heartbeat_at.lock().await =
            Some(Instant::now() - (HEARTBEAT_TIMEOUT + Duration::from_secs(1)));
        assert!(!c.mavlink_alive().await, "stale heartbeat → not alive");
        assert!(!c.connected().await);
    }

    #[tokio::test]
    async fn link_hint_is_none_when_alive_or_no_transport() {
        let c = conn_with(MavlinkConfig::default());
        // Fresh connection: transport closed → nothing to explain.
        assert_eq!(c.link_hint().await, "none");
        // Transport open with a fresh heartbeat → alive → still none.
        c.connected.store(true, Ordering::Relaxed);
        *c.last_heartbeat_at.lock().await = Some(Instant::now());
        assert_eq!(c.link_hint().await, "none");
    }

    #[tokio::test]
    async fn link_hint_is_no_heartbeat_when_open_and_silent() {
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        // Transport open, no heartbeat, no MSP evidence → no_heartbeat.
        assert_eq!(c.link_hint().await, "no_heartbeat");
    }

    #[tokio::test]
    async fn link_hint_is_msp_detected_with_recent_evidence_and_clears_on_heartbeat() {
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        *c.last_msp_at.lock().await = Some(Instant::now());
        assert_eq!(c.link_hint().await, "msp_detected");
        // A fresh HEARTBEAT (alive) overrides the MSP hint immediately.
        *c.last_heartbeat_at.lock().await = Some(Instant::now());
        assert_eq!(c.link_hint().await, "none");
    }

    #[tokio::test]
    async fn link_hint_msp_evidence_ages_out() {
        let c = conn_with(MavlinkConfig::default());
        c.connected.store(true, Ordering::Relaxed);
        *c.last_msp_at.lock().await =
            Some(Instant::now() - (MSP_HINT_TTL + Duration::from_secs(1)));
        // Stale MSP evidence no longer arms the hint; falls back to no_heartbeat.
        assert_eq!(c.link_hint().await, "no_heartbeat");
    }

    #[test]
    fn fc_heartbeat_is_recognized_when_it_shares_the_agent_system_id() {
        // The standard ArduPilot companion config: FC sysid 1 / compid 1, agent
        // companion sysid 1 / compid 191. The FC heartbeat SHARES the agent's
        // system_id but differs by component_id, so it must NOT be filtered as
        // our own — else the link never reads alive while the FC streams.
        let c = conn_with(MavlinkConfig::default());
        assert_eq!(c.cfg.system_id, 1, "default companion system_id is 1");
        assert_ne!(
            c.cfg.component_id, 1,
            "the companion component_id must differ from the autopilot's (1)"
        );
        // The FC's own heartbeat (autopilot component 1) is NOT ours → counted.
        assert!(!c.is_own_heartbeat(1, 1));
        // Our own companion heartbeat (matching sysid AND compid) is filtered.
        assert!(c.is_own_heartbeat(c.cfg.system_id, c.cfg.component_id));
        // A heartbeat from a different system is also not ours.
        assert!(!c.is_own_heartbeat(2, 1));
    }

    #[test]
    fn source_infers_from_serial_port_shape_when_unset() {
        // An empty `source` (a config that predates the field) is inferred from
        // the connection-string shape so the picker still reflects reality.
        // Empty port → auto-detect.
        assert_eq!(
            conn_source(MavlinkConfig {
                source: String::new(),
                serial_port: String::new(),
                ..MavlinkConfig::default()
            }),
            "auto"
        );
        // A udp:/tcp: prefix is a network transport.
        assert_eq!(
            conn_source(MavlinkConfig {
                source: String::new(),
                serial_port: "udp:127.0.0.1:14550".into(),
                ..MavlinkConfig::default()
            }),
            "udp"
        );
        assert_eq!(
            conn_source(MavlinkConfig {
                source: String::new(),
                serial_port: "tcp:127.0.0.1:5760".into(),
                ..MavlinkConfig::default()
            }),
            "tcp"
        );
        // A bare device path is serial.
        assert_eq!(
            conn_source(MavlinkConfig {
                source: String::new(),
                serial_port: "/dev/ttyACM0".into(),
                ..MavlinkConfig::default()
            }),
            "serial"
        );
    }

    #[test]
    fn source_honors_an_explicit_value() {
        // An explicit pick is reported verbatim (NOT re-inferred): "auto" means
        // the operator chose agent-decides, even with a serial_port present.
        assert_eq!(
            conn_source(MavlinkConfig {
                source: "auto".into(),
                serial_port: "/dev/ttyACM0".into(),
                ..MavlinkConfig::default()
            }),
            "auto"
        );
        for kind in ["serial", "udp", "tcp", "auto"] {
            assert_eq!(
                conn_source(MavlinkConfig {
                    source: kind.into(),
                    ..MavlinkConfig::default()
                }),
                kind
            );
        }
    }

    fn conn_source(cfg: MavlinkConfig) -> &'static str {
        conn_with(cfg).source()
    }
}
