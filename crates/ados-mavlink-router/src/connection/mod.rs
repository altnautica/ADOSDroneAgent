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
//! MAVLink-over-ELRS ingest: when the `radio.crsf` block declares the RC
//! module runs its native MAVLink mode, the module is a MAVLink byte carrier
//! (the module firmware owns the CRSF air protocol internally — nothing here
//! parses CRSF) and THIS router ingests the carrier as its FC source: the
//! pinned serial device at the fixed MAVLink-mode baud, or a UDP listen on the
//! conventional port for the WiFi backpack. The resolved source replaces both
//! the configured port and discovery — the RC lane service holds off the
//! device in that mode, so the port has exactly one owner, and a fallback
//! sweep would latch some other port while the module sat unused. In
//! `crsf_rc` mode the ownership inverts: the RC lane service owns the pin and
//! [`FcConnection::candidate_ports`] excludes it.
//!
//! This source's direction is asymmetric by default. The router reads inbound
//! MAVLink (telemetry up, so the drone appears and telemetry flows), but the
//! host->FC command-down direction is GATED CLOSED — [`FcConnection::run`]
//! installs no writer for a MAVLink-over-ELRS source until
//! `radio.crsf.mavlink_command_enabled` is set (see
//! [`FcConnection::command_down_gated`]). With the marker off (the default and
//! only current state) the source is telemetry-only; every other FC source
//! (serial / UDP / TCP / discovery) keeps its full command path.
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

use crate::config::{CrsfMavlinkSource, MavlinkConfig, CRSF_MAVLINK_BAUD};
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

use ados_protocol::hwcaps::is_rc_bridge_usb_id;
use framing::{count_msp_frame_starts, extract_frames};
use send_scheduler::STREAM_DEFAULT;
use transport::{
    fc_variant_for_port, is_candidate_port, now_iso, open_serial, open_udp_listen, parse_net_spec,
    persist_params, probe_baud, same_device, split_serial, BoxedReadHalf, BoxedWriteHalf, NetSpec,
    ProbeOutcome, UdpAdapter, BAUD_CANDIDATES, BAUD_FALLBACK,
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
    /// Raw FC->host byte lane. Populated only for an MSP FC (Betaflight/iNav),
    /// whose FC->host bytes are MSP responses, not MAVLink frames, so they never
    /// appear on `frame_tx` (extract_frames yields nothing). Empty for a MAVLink
    /// FC. The direct-GCS proxies subscribe to both lanes and forward whichever
    /// carries bytes, so a polling MSP GCS receives the FC's responses while the
    /// MAVLink frame path stays byte-unchanged.
    raw_tx: broadcast::Sender<Vec<u8>>,
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
    /// True when the most recent open attempt FAILED to establish a transport
    /// (the socket/serial device could not be opened at all). Drives the
    /// `source_unreachable` link hint: a configured (non-auto) source whose
    /// endpoint never opens is an unreachable / wrong endpoint, not merely a
    /// silent FC. Set false the moment a transport opens; the run loop owns it.
    open_failed: AtomicBool,
    port: Mutex<String>,
    /// The FC firmware family identified from the opened port's USB descriptor
    /// (`betaflight` / `inav`), or `None` for a MAVLink / unknown FC. An MSP FC
    /// (Betaflight/iNav) is silent over USB until polled, so it never emits the
    /// heartbeat the alive gate needs; the USB product string is the passive,
    /// reliable signal that the attached FC is an MSP board. Recomputed on every
    /// open, cleared on teardown.
    fc_variant: Mutex<Option<String>>,
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
        let (raw_tx, _) = broadcast::channel(FRAME_CHANNEL_CAP);
        std::sync::Arc::new(Self {
            cfg,
            state,
            params,
            frame_tx,
            raw_tx,
            writer: Mutex::new(None),
            reconnect: tokio::sync::Notify::new(),
            seq: AtomicU8::new(0),
            target_system: AtomicU8::new(1),
            connected: AtomicBool::new(false),
            wrote_since_open: AtomicBool::new(false),
            open_failed: AtomicBool::new(false),
            port: Mutex::new(String::new()),
            fc_variant: Mutex::new(None),
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

    /// Subscribe to the raw inbound FC byte lane (populated only for an MSP FC).
    pub fn subscribe_raw(&self) -> broadcast::Receiver<Vec<u8>> {
        self.raw_tx.subscribe()
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
    ///   - `source_unreachable` — the transport is NOT open and a configured
    ///     (non-auto) source failed to open: the configured endpoint (a wrong /
    ///     down `tcp:`/`udp:` host or an absent serial device) is unreachable.
    ///     The operator fix is the source config, not plugging in an FC.
    ///
    /// Derived (not latched) so it self-corrects: the moment a HEARTBEAT decodes
    /// it reads `none`, the MSP evidence ages out via [`MSP_HINT_TTL`], and the
    /// unreachable flag clears the moment a transport opens.
    pub async fn link_hint(&self) -> &'static str {
        if &*self.port.lock().await == "demo" {
            return "none";
        }
        if self.mavlink_alive().await {
            return "none";
        }
        if !self.transport_open() {
            // A configured (non-auto) source whose last open attempt failed is
            // an unreachable / wrong endpoint — the operator's fix is the source
            // config, not "plug in an FC". Auto-discovery stays silent: with no
            // configured endpoint there is nothing to be unreachable.
            if self.open_failed.load(Ordering::Relaxed) && self.source() != "auto" {
                return "source_unreachable";
            }
            return "none";
        }
        // A Betaflight/iNav FC identified by its USB descriptor speaks MSP, not
        // MAVLink, and is silent until polled — so it never trips the passive
        // byte sniff below. The descriptor is the reliable signal that "no
        // heartbeat" is really "this is an MSP FC", not a broken link.
        if self.fc_variant.lock().await.is_some() {
            return "msp_detected";
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

    /// The FC firmware family identified from the opened port's USB descriptor
    /// (`betaflight` / `inav`), or `None` for a MAVLink / unknown FC.
    pub async fn fc_variant(&self) -> Option<String> {
        self.fc_variant.lock().await.clone()
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
    ///
    /// A resolved MAVLink-over-ELRS ingest owns the source slot first and
    /// reports its live carrier class — `serial` for the module's USB-serial
    /// port, `udp` for the WiFi-backpack listen. Both are true statements
    /// about the open transport; the provenance (the `radio.crsf` block) is
    /// visible on the port label (`fc_port` = the pinned device / the
    /// `udpin:` bind) and on the RC lane's own status surface.
    pub fn source(&self) -> &'static str {
        match self.cfg.crsf_mavlink_source() {
            Some(CrsfMavlinkSource::Serial { .. }) => return "serial",
            Some(CrsfMavlinkSource::BackpackUdp { .. }) => return "udp",
            None => {}
        }
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
    /// Whether the resolved FC source is the MAVLink-over-ELRS ingest with its
    /// host->FC command-down direction gated closed (telemetry-only). True
    /// ONLY when the source is a [`CrsfMavlinkSource`] AND the explicit
    /// `radio.crsf.mavlink_command_enabled` marker is off — the default and
    /// only current state. Every other FC source (serial / UDP / TCP /
    /// discovery) returns false, so their command paths are untouched. When
    /// true, [`Self::run`] installs no writer, so the send scheduler's
    /// heartbeat, stream requests, param sweep, and any forwarded client bytes
    /// are all suppressed for this source (send_bytes is a no-op with no
    /// writer). Setting the marker flips this false, restoring the writer for
    /// the bench gate. Because `open()` resolves a `CrsfMavlinkSource` iff
    /// `crsf_mavlink_source()` is `Some`, this predicate is exactly "the open
    /// transport is that source AND its command marker is off".
    fn command_down_gated(&self) -> bool {
        self.cfg.crsf_mavlink_source().is_some() && !self.cfg.crsf_mavlink_command_enabled
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
                // The transport could not be opened — the configured endpoint is
                // unreachable / absent. Record it so link_hint() can explain the
                // silent link instead of leaving the operator with a bare "not
                // connected".
                self.open_failed.store(true, Ordering::Relaxed);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.notified() => return,
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
                continue;
            };
            // A transport opened — clear the unreachable flag.
            self.open_failed.store(false, Ordering::Relaxed);
            *self.port.lock().await = port.clone();
            // Identify an MSP FC (Betaflight/iNav) by the opened port's USB
            // descriptor — the passive signal that survives the silent-until-
            // polled blind spot of the byte sniff.
            *self.fc_variant.lock().await = fc_variant_for_port(&port);
            self.baud.store(baud, Ordering::Relaxed);
            if self.command_down_gated() {
                // MAVLink-over-ELRS, telemetry-only: install NO writer. With no
                // writer every send path is a no-op (send_bytes early-returns),
                // so the companion heartbeat, stream-interval requests, param
                // sweep, and any forwarded client bytes are all suppressed and
                // the host->FC command direction stays closed over the RC lane
                // — while the read loop below still ingests inbound telemetry.
                // Dropping the write half closes the send side outright, a
                // stronger guarantee than gating each send path since no code
                // can smuggle a byte through an absent writer. Enabling the
                // command marker restores the writer (the else branch) for the
                // bench-validated command lane.
                drop(write_half);
                *self.writer.lock().await = None;
                tracing::info!(port = %port, baud, "fc_connected_crsf_mavlink_command_down_gated");
            } else {
                *self.writer.lock().await = Some(write_half);
                tracing::info!(port = %port, baud, "fc_connected");
            }
            self.connected.store(true, Ordering::Relaxed);
            self.wrote_since_open.store(false, Ordering::Relaxed);
            *self.last_msg_at.lock().await = Instant::now();

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
            *self.fc_variant.lock().await = None;
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
        // An MSP FC (identified by its USB descriptor at open) speaks MSP, not
        // MAVLink, on FC->host, so extract_frames yields nothing and its responses
        // never reach a GCS proxy on the frame lane. The variant is fixed for this
        // read loop (set before it starts, cleared only after it returns), so
        // capture it once and forward the raw bytes below. A MAVLink FC captures
        // false here, so the frame path is byte-unchanged.
        let is_msp = self.fc_variant.lock().await.is_some();
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
            // An MSP FC speaks MSP (not MAVLink) on FC->host: forward the raw chunk
            // verbatim so a polling MSP GCS receives the FC's responses, and skip
            // MAVLink framing entirely. extract_frames does no CRC check and would
            // carve garbage "frames" from MSP payload bytes that contain 0xFD/0xFE,
            // so running it for an MSP FC would fan corrupt bytes onto the frame
            // lane alongside the raw ones. is_msp is false for a MAVLink FC, so the
            // framing path below is byte-unchanged; the MSP link hint comes from the
            // USB-descriptor variant, so the MSP-start sniff is unnecessary here too.
            if is_msp {
                let _ = self.raw_tx.send(chunk[..n].to_vec());
                continue;
            }
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
        // MAVLink-over-ELRS ingest: a resolved source REPLACES the configured
        // port and discovery entirely. `radio.crsf.mode: mavlink` is the
        // operator's explicit statement that the RC module is the MAVLink
        // bearer (the same precedence the pin already wins over a
        // contradictory `mavlink.serial_port` in RC mode), and falling
        // through on an open failure would latch some other port while the
        // module sat unused. A failed open is a failed attempt: the run
        // loop's backoff owns the retry and the `source_unreachable` hint
        // names the cause. No CRSF is parsed on this lane — the module
        // firmware owns the air protocol; the host side is plain MAVLink
        // bytes through the ordinary read loop and its heartbeat gate.
        if let Some(src) = self.cfg.crsf_mavlink_source() {
            return match src {
                CrsfMavlinkSource::Serial { device } => {
                    let device = device.to_string();
                    match open_serial(&device, CRSF_MAVLINK_BAUD) {
                        Some(stream) => Some(split_serial(stream, device, CRSF_MAVLINK_BAUD)),
                        None => {
                            tracing::warn!(device = %device, "fc_crsf_mavlink_serial_open_failed");
                            None
                        }
                    }
                }
                CrsfMavlinkSource::BackpackUdp { port } => open_udp_listen(port).await,
            };
        }

        // SITL / network transport: detected from the configured connection
        // string, never from serial discovery (baud is not meaningful here).
        let configured = self.cfg.serial_port.trim();
        if let Some(spec) = parse_net_spec(configured) {
            return self.open_net(spec).await;
        }

        let candidates = self.candidate_ports();
        for cand in candidates {
            let port = cand.name;
            // A configured baud skips the probe; otherwise probe the candidates.
            if self.cfg.baud_rate != 0 && !self.cfg.serial_port.is_empty() {
                if let Some(stream) = open_serial(&port, self.cfg.baud_rate) {
                    return Some(split_serial(stream, port, self.cfg.baud_rate));
                }
                continue;
            }
            // Whether the sweep heard a talking FC (MAVLink or MSP) on this
            // port, even if the follow-up open failed.
            let mut sweep_heard_fc = false;
            for &baud in BAUD_CANDIDATES {
                match probe_baud(&port, baud).await {
                    ProbeOutcome::Heartbeat => {
                        sweep_heard_fc = true;
                        if let Some(stream) = open_serial(&port, baud) {
                            return Some(split_serial(stream, port, baud));
                        }
                    }
                    ProbeOutcome::Msp => {
                        // The FC is emitting MSP, not MAVLink — no baud will yield
                        // a HEARTBEAT. Open here so the read loop surfaces the
                        // msp_detected hint, and stop sweeping the remaining bauds.
                        sweep_heard_fc = true;
                        if let Some(stream) = open_serial(&port, baud) {
                            return Some(split_serial(stream, port, baud));
                        }
                        break;
                    }
                    ProbeOutcome::None => {}
                }
            }
            // A silent port behind a known RC-bridge USB id (CP2102 / CH340 /
            // ESP32-S3 — the bridges an ExpressLRS TX module enumerates behind)
            // is NOT latched by the no-evidence fallback open: doing so pins the
            // router to the RC module at the fallback baud and the real FC (which
            // may enumerate later in the list) is never reached. The sweep above
            // is the sanity check that keeps an FC behind the same bridge alive —
            // a live FC proves itself with a HEARTBEAT or MSP traffic, and any
            // other vendor keeps the fallback exactly as before.
            if !sweep_heard_fc {
                if let Some((vid, pid)) = cand.usb {
                    if is_rc_bridge_usb_id(vid, pid) {
                        tracing::info!(
                            port = %port,
                            vid = format_args!("{vid:04x}"),
                            pid = format_args!("{pid:04x}"),
                            "silent_rc_bridge_fallback_skipped"
                        );
                        continue;
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

    /// The serial ports the FC link may open, with the pinned CRSF/ELRS device
    /// (`radio.crsf.device`) excluded on BOTH paths: an RC transmitter module's
    /// port must never be opened or baud-swept by the router, even when the FC
    /// port is (mis)configured to the same node — the pin is the operator's
    /// explicit statement that the device is the RC module, so it wins over a
    /// contradictory `mavlink.serial_port`.
    fn candidate_ports(&self) -> Vec<FcCandidate> {
        let crsf_pin = self.cfg.crsf_device.trim();
        if !self.cfg.serial_port.is_empty() {
            if !crsf_pin.is_empty() && same_device(&self.cfg.serial_port, crsf_pin) {
                tracing::warn!(
                    port = %self.cfg.serial_port,
                    "fc_serial_port_is_the_pinned_crsf_device_no_fc_candidates"
                );
                return Vec::new();
            }
            return vec![FcCandidate {
                name: self.cfg.serial_port.clone(),
                usb: None,
            }];
        }
        match tokio_serial::available_ports() {
            Ok(ports) => ports
                .into_iter()
                .filter(|p| is_candidate_port(&p.port_type, &p.port_name))
                .filter(|p| crsf_pin.is_empty() || !same_device(&p.port_name, crsf_pin))
                .map(|p| FcCandidate {
                    usb: match &p.port_type {
                        tokio_serial::SerialPortType::UsbPort(info) => Some((info.vid, info.pid)),
                        _ => None,
                    },
                    name: p.port_name,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// A serial candidate for the FC link: the device path plus, when it came from
/// USB enumeration, the backing USB id. The pinned-port path carries no id (an
/// explicitly configured FC port is opened as configured, no vendor gating).
struct FcCandidate {
    name: String,
    usb: Option<(u16, u16)>,
}

#[cfg(test)]
mod crsf_exclusion_tests {
    use super::*;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;

    fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-crsf-exclusion-params.json",
        )));
        FcConnection::new(cfg, state, params)
    }

    #[test]
    fn pinned_fc_port_survives_when_distinct_from_the_crsf_pin() {
        let c = conn_with(MavlinkConfig {
            serial_port: "/dev/ttyACM0".into(),
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        });
        let cands = c.candidate_ports();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].name, "/dev/ttyACM0");
        // An explicitly configured port carries no USB id (no vendor gating).
        assert_eq!(cands[0].usb, None);
    }

    #[test]
    fn crsf_pin_excludes_a_matching_configured_fc_port() {
        // The pin is the operator's explicit statement that the device is the
        // RC module; a contradictory FC config must not open it.
        let c = conn_with(MavlinkConfig {
            serial_port: "/dev/ttyUSB0".into(),
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        });
        assert!(c.candidate_ports().is_empty());
    }

    #[test]
    fn no_pin_keeps_the_configured_port() {
        let c = conn_with(MavlinkConfig {
            serial_port: "/dev/ttyACM0".into(),
            ..MavlinkConfig::default()
        });
        assert_eq!(c.cfg.crsf_device, "");
        let cands = c.candidate_ports();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].name, "/dev/ttyACM0");
    }

    /// A resolved MAVLink-over-ELRS ingest owns the FC source slot: the
    /// snapshot reports the live carrier class (`serial` / `udp`), not the
    /// operator's `mavlink.source` pick, while an unresolved lane leaves the
    /// slot to the ordinary config path.
    #[test]
    fn crsf_mavlink_mode_owns_the_fc_source_slot() {
        let serial = conn_with(MavlinkConfig {
            crsf_enabled: true,
            crsf_mode: "mavlink".into(),
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        });
        assert_eq!(serial.source(), "serial");

        let backpack = conn_with(MavlinkConfig {
            crsf_enabled: true,
            crsf_mode: "mavlink".into(),
            crsf_mavlink_transport: "backpack_wifi".into(),
            ..MavlinkConfig::default()
        });
        assert_eq!(backpack.source(), "udp");

        // RC mode (the default): the pin is an exclusion, never a source —
        // the slot falls through to the configured/auto vocabulary.
        let rc = conn_with(MavlinkConfig {
            crsf_enabled: true,
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        });
        assert_eq!(rc.cfg.crsf_mavlink_source(), None);
        assert_eq!(rc.source(), "auto");

        // MAVLink mode on the serial carrier with no pin: unresolved — the
        // slot is not claimed and discovery vocabulary stands.
        let unpinned = conn_with(MavlinkConfig {
            crsf_enabled: true,
            crsf_mode: "mavlink".into(),
            ..MavlinkConfig::default()
        });
        assert_eq!(unpinned.cfg.crsf_mavlink_source(), None);
        assert_eq!(unpinned.source(), "auto");
    }
}

#[cfg(test)]
mod command_gate_tests {
    use super::*;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-crsf-cmd-gate-params.json",
        )));
        FcConnection::new(cfg, state, params)
    }

    /// A write half that counts the bytes handed to it, standing in for the FC
    /// carrier so a test can prove whether a send path reached the FC.
    struct CountingWriter(std::sync::Arc<AtomicUsize>);
    impl AsyncWrite for CountingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.fetch_add(data.len(), Ordering::Relaxed);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The gated default: the lane opted in, MAVLink mode, a pinned serial
    /// carrier, and NO command marker — a live MAVLink-over-ELRS source that is
    /// telemetry-only.
    fn gated_mavlink_cfg() -> MavlinkConfig {
        MavlinkConfig {
            crsf_enabled: true,
            crsf_mode: "mavlink".into(),
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        }
    }

    #[test]
    fn a_crsf_mavlink_source_defaults_to_command_down_gated() {
        let c = conn_with(gated_mavlink_cfg());
        assert!(c.cfg.crsf_mavlink_source().is_some(), "the source resolves");
        assert!(
            c.command_down_gated(),
            "marker off + crsf mavlink source → command-down gated"
        );
    }

    #[test]
    fn the_command_marker_restores_the_writer_path() {
        // Flipping the marker on is the bench-gate switch: the same source is
        // no longer gated, so run() installs a writer for the command lane.
        let mut cfg = gated_mavlink_cfg();
        cfg.crsf_mavlink_command_enabled = true;
        let c = conn_with(cfg);
        assert!(c.cfg.crsf_mavlink_source().is_some());
        assert!(
            !c.command_down_gated(),
            "marker on → writer restored (not gated)"
        );
    }

    #[test]
    fn other_fc_sources_are_never_command_down_gated() {
        // A plain serial FC is not a CRSF source, so it is never gated — its
        // command path stays open regardless of the marker's value.
        for marker in [false, true] {
            let serial = conn_with(MavlinkConfig {
                serial_port: "/dev/ttyACM0".into(),
                crsf_mavlink_command_enabled: marker,
                ..MavlinkConfig::default()
            });
            assert_eq!(serial.cfg.crsf_mavlink_source(), None);
            assert!(!serial.command_down_gated(), "marker={marker}");
        }
        // A UDP SITL transport: same — not a CRSF source, never gated.
        let udp = conn_with(MavlinkConfig {
            source: "udp".into(),
            serial_port: "udp:127.0.0.1:14550".into(),
            ..MavlinkConfig::default()
        });
        assert!(!udp.command_down_gated());
    }

    #[tokio::test]
    async fn a_gated_source_installs_no_writer_and_suppresses_every_send_path() {
        // Mirror run()'s gated branch: a gated source installs no writer. With
        // the transport open (so the scheduler bodies run) the companion
        // heartbeat, the stream requests, and a forwarded client command all
        // reach a NULL writer — nothing is transmitted toward the FC, and no
        // reconnect is raised (there was no writer to fail). Inbound telemetry
        // is unaffected: the read loop never touches the writer.
        let c = conn_with(gated_mavlink_cfg());
        assert!(c.command_down_gated());
        c.connected.store(true, Ordering::Relaxed);
        assert!(
            c.writer.lock().await.is_none(),
            "the gated branch installs no writer"
        );
        c.send_heartbeat().await;
        c.tick_streams().await;
        c.send_bytes(b"\xfd\x00\x00\x00").await;
        assert!(
            c.writer.lock().await.is_none(),
            "no send path creates a writer for a gated source"
        );
        let signalled = tokio::time::timeout(Duration::from_millis(50), c.reconnect.notified())
            .await
            .is_ok();
        assert!(
            !signalled,
            "a gated source with no writer raises no reconnect"
        );
    }

    #[tokio::test]
    async fn an_ungated_source_writes_the_companion_heartbeat_to_the_fc() {
        // The other side of the gate: a non-CRSF FC source installs a writer,
        // so the send scheduler reaches the FC. Count the heartbeat bytes that
        // land on the writer to prove the command path still works — the WFB /
        // serial FC path must be unaffected by the CRSF gate.
        let c = conn_with(MavlinkConfig {
            serial_port: "/dev/ttyACM0".into(),
            ..MavlinkConfig::default()
        });
        assert!(!c.command_down_gated());
        let written = std::sync::Arc::new(AtomicUsize::new(0));
        c.connected.store(true, Ordering::Relaxed);
        *c.writer.lock().await = Some(Box::pin(CountingWriter(written.clone())));
        c.send_heartbeat().await;
        assert!(
            written.load(Ordering::Relaxed) > 0,
            "the FC command path wrote the heartbeat"
        );
        assert!(c.wrote_since_open.load(Ordering::Relaxed));
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

    #[tokio::test]
    async fn link_hint_is_source_unreachable_for_a_failed_configured_source() {
        // A configured tcp source whose endpoint never opens (the run loop set
        // open_failed) is unreachable, not merely a silent FC.
        let c = conn_with(MavlinkConfig {
            source: "tcp".into(),
            serial_port: "tcp:203.0.113.9:15760".into(),
            ..MavlinkConfig::default()
        });
        c.open_failed.store(true, Ordering::Relaxed);
        assert!(!c.transport_open());
        assert_eq!(c.link_hint().await, "source_unreachable");
    }

    #[tokio::test]
    async fn link_hint_auto_discovery_stays_none_even_when_open_fails() {
        // Auto-discovery has no configured endpoint to be "unreachable", so a
        // failed probe stays quiet (nothing for the operator to fix).
        let c = conn_with(MavlinkConfig::default());
        c.open_failed.store(true, Ordering::Relaxed);
        assert_eq!(c.source(), "auto");
        assert_eq!(c.link_hint().await, "none");
    }

    #[tokio::test]
    async fn link_hint_clears_source_unreachable_once_a_transport_opens() {
        let c = conn_with(MavlinkConfig {
            source: "tcp".into(),
            serial_port: "tcp:203.0.113.9:15760".into(),
            ..MavlinkConfig::default()
        });
        c.open_failed.store(true, Ordering::Relaxed);
        assert_eq!(c.link_hint().await, "source_unreachable");
        // The transport opened (run loop clears open_failed); the link is now
        // open-and-silent, so the hint moves to no_heartbeat.
        c.open_failed.store(false, Ordering::Relaxed);
        c.connected.store(true, Ordering::Relaxed);
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

#[cfg(test)]
mod passthrough_tests {
    //! Transparent MSP passthrough: for an MSP FC (Betaflight/iNav) the raw
    //! FC->host bytes travel on the raw lane so a polling MSP GCS receives the
    //! FC's responses, while a MAVLink FC's frame path stays byte-unchanged and
    //! never touches the raw lane.
    use super::*;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    use tokio::sync::broadcast::error::TryRecvError;

    fn conn() -> std::sync::Arc<FcConnection> {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-passthrough-params.json",
        )));
        FcConnection::new(MavlinkConfig::default(), state, params)
    }

    /// A real MAVLink v2 HEARTBEAT frame (same constructor as the framing tests).
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

    /// A reader that yields its pre-set chunks one poll at a time, then EOF. Lets
    /// a test drive `read_loop` with a frame split across two reads to exercise
    /// the reassembly path.
    struct ChunkReader {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }
    impl ChunkReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into_iter().collect(),
            }
        }
    }
    impl tokio::io::AsyncRead for ChunkReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if let Some(chunk) = this.chunks.pop_front() {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    this.chunks.push_front(chunk[n..].to_vec());
                }
            }
            // No chunk left (or a zero-remaining buffer): 0 bytes filled reads as
            // EOF, so `read_loop` returns rather than hanging.
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn msp_fc_raw_bytes_reach_the_raw_lane_and_not_the_frame_lane() {
        let c = conn();
        // As fc_variant_for_port would set it at open for a Betaflight board.
        *c.fc_variant.lock().await = Some("betaflight".into());
        let mut raw_rx = c.subscribe_raw();
        let mut frame_rx = c.subscribe();
        // A representative MSP `$M>` response with no MAVLink magic (0xFD/0xFE).
        let msp = b"\x24\x4d\x3e\x02\x64\x01\x02\x67".to_vec();
        c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
            .await;
        // The exact raw bytes are forwarded on the raw lane...
        assert_eq!(raw_rx.try_recv().unwrap(), msp);
        // ...and the frame lane stays silent (MSP yields no MAVLink frame).
        assert!(matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn mavlink_fc_frame_path_is_unchanged_and_raw_lane_is_silent() {
        let c = conn();
        // A MAVLink FC has no USB-descriptor variant → is_msp is false.
        assert!(c.fc_variant.lock().await.is_none());
        let mut raw_rx = c.subscribe_raw();
        let mut frame_rx = c.subscribe();
        let frame = heartbeat_frame();
        c.read_loop(Box::pin(std::io::Cursor::new(frame.clone())))
            .await;
        // The exact frame bytes, verbatim, no re-encode.
        assert_eq!(frame_rx.try_recv().unwrap(), frame);
        // The raw lane never fires for a MAVLink FC.
        assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn a_mavlink_frame_split_across_two_reads_lands_once_on_the_frame_lane() {
        let c = conn();
        let mut raw_rx = c.subscribe_raw();
        let mut frame_rx = c.subscribe();
        let frame = heartbeat_frame();
        let split = frame.len() / 2;
        let reader = ChunkReader::new(vec![frame[..split].to_vec(), frame[split..].to_vec()]);
        c.read_loop(Box::pin(reader)).await;
        // Reassembled across the two reads into exactly one frame.
        assert_eq!(frame_rx.try_recv().unwrap(), frame);
        assert!(
            matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)),
            "exactly one frame, not duplicated"
        );
        assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn an_msp_chunk_with_an_embedded_frame_start_is_forwarded_verbatim_once() {
        let c = conn();
        *c.fc_variant.lock().await = Some("inav".into());
        let mut raw_rx = c.subscribe_raw();
        // A chunk that embeds a stray `$M>` mid-stream is still forwarded whole.
        let chunk = b"\x01\x02\x24\x4d\x3e\x05\x06".to_vec();
        c.read_loop(Box::pin(std::io::Cursor::new(chunk.clone())))
            .await;
        assert_eq!(raw_rx.try_recv().unwrap(), chunk);
        assert!(
            matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)),
            "exactly one raw chunk"
        );
    }

    #[tokio::test]
    async fn an_msp_chunk_containing_a_mavlink_magic_byte_never_carves_a_garbage_frame() {
        let c = conn();
        *c.fc_variant.lock().await = Some("betaflight".into());
        let mut raw_rx = c.subscribe_raw();
        let mut frame_rx = c.subscribe();
        // An MSP response whose payload contains the MAVLink v2 magic 0xFD
        // followed by a plausible length byte. extract_frames does NO CRC check,
        // so without the MSP fast-path it would carve a garbage "frame" from
        // these bytes and fan it onto the frame lane alongside the raw copy,
        // corrupting the MSP stream a GCS sees. The frame lane MUST stay silent.
        let mut msp = b"\x24\x4d\x3e\x08\xfd\x00\x00".to_vec();
        msp.extend(std::iter::repeat_n(0xAA, 20));
        c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
            .await;
        // The full MSP chunk is forwarded once, verbatim, on the raw lane.
        assert_eq!(raw_rx.try_recv().unwrap(), msp);
        assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
        // No garbage frame is ever carved onto the frame lane for an MSP FC.
        assert!(
            matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)),
            "an MSP FC must never emit a MAVLink frame, even when its payload contains 0xFD/0xFE"
        );
    }

    /// End-to-end over the two byte-lane IPC sockets the router serves: an MSP FC's
    /// FC->host bytes reach a client on the MSP socket (via the raw lane producer),
    /// while a client on the MAVLink socket stays silent (the frame lane never
    /// fires for an MSP FC). Stands up both sockets, wires the two producers exactly
    /// as the router's `main()`, drives a real MSP `read_loop`, and observes both.
    #[tokio::test]
    async fn the_msp_socket_carries_raw_bytes_while_the_mavlink_socket_stays_silent() {
        use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
        use ados_protocol::ipc::{connect_with_retry, read_length_prefixed, IpcBroadcast};

        let dir = std::env::temp_dir();
        let msp_path = dir.join(format!("ados-mspplane-{}.sock", std::process::id()));
        let mav_path = dir.join(format!("ados-mavplane-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&msp_path);
        let _ = std::fs::remove_file(&mav_path);

        // Both sockets as the router binds them: length-prefixed, 256-deep, with an
        // inbound channel (unused here — this test exercises only the FC->client path).
        let (msp_ipc, _msp_inbound) = IpcBroadcast::bind(&msp_path, 256, false, Some(256))
            .await
            .unwrap();
        let msp_ipc = std::sync::Arc::new(msp_ipc);
        let (mav_ipc, _mav_inbound) = IpcBroadcast::bind(&mav_path, 256, false, Some(256))
            .await
            .unwrap();
        let mav_ipc = std::sync::Arc::new(mav_ipc);

        // An MSP FC, so the read loop routes FC->host bytes onto the raw lane.
        let c = conn();
        *c.fc_variant.lock().await = Some("betaflight".into());

        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        // Raw lane -> MSP socket (the new producer, verbatim from `main()`).
        {
            let msp_ipc = msp_ipc.clone();
            let cancel = cancel.clone();
            let mut raw_rx = c.subscribe_raw();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        chunk = raw_rx.recv() => match chunk {
                            Ok(bytes) => {
                                if let Ok(framed) = encode_frame(&bytes, MAVLINK_MAX_FRAME) {
                                    msp_ipc.broadcast(framed).await;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                        _ = cancel.notified() => break,
                    }
                }
            });
        }
        // Frame lane -> MAVLink socket (the existing producer, verbatim from `main()`).
        {
            let mav_ipc = mav_ipc.clone();
            let cancel = cancel.clone();
            let mut frame_rx = c.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        frame = frame_rx.recv() => match frame {
                            Ok(f) => {
                                if let Ok(framed) = encode_frame(&f, MAVLINK_MAX_FRAME) {
                                    mav_ipc.broadcast(framed).await;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                        _ = cancel.notified() => break,
                    }
                }
            });
        }

        // A client on each socket, registered before any broadcast fires.
        let mut msp_client = connect_with_retry(&msp_path, 20, Duration::from_millis(20))
            .await
            .unwrap();
        let mut mav_client = connect_with_retry(&mav_path, 20, Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drive the MSP FC: read_loop forwards the raw chunk onto the raw lane.
        let msp = b"\x24\x4d\x3e\x02\x64\x01\x02\x67".to_vec();
        c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
            .await;

        // The MSP socket client receives the exact MSP bytes (length-prefixed on the
        // wire, de-framed back to the original chunk).
        let got = tokio::time::timeout(
            Duration::from_secs(1),
            read_length_prefixed(&mut msp_client, MAVLINK_MAX_FRAME, false),
        )
        .await
        .expect("the msp client read must not time out")
        .unwrap();
        assert_eq!(got.as_deref(), Some(&msp[..]));

        // The MAVLink socket client stays silent: an MSP FC never puts a frame on
        // the frame lane, so nothing is ever broadcast to it.
        let silent = tokio::time::timeout(
            Duration::from_millis(200),
            read_length_prefixed(&mut mav_client, MAVLINK_MAX_FRAME, false),
        )
        .await;
        assert!(
            silent.is_err(),
            "the mavlink socket must stay silent for an MSP FC"
        );

        cancel.notify_waiters();
        let _ = std::fs::remove_file(&msp_path);
        let _ = std::fs::remove_file(&mav_path);
    }
}
