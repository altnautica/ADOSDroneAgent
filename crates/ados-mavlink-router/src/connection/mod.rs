//! Flight-controller serial link.
//!
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
pub mod imu_fallback;
pub mod injector_gate;
mod link_state;
mod send_scheduler;
pub use send_scheduler::ClientOrigin;
pub(crate) use send_scheduler::FC_MAX_SINGLE_WRITE;
pub mod swarm_setpoint;
pub(crate) mod transport;
mod visor_gate;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::{self, MavHeader};
use ados_protocol::shutdown::Shutdown;
use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, Mutex};

use crate::config::{CrsfMavlinkSource, MavlinkConfig, CRSF_MAVLINK_BAUD};
use crate::param_cache::ParamCache;
use crate::rx_frame::RxFrame;
use crate::state::{VehicleSource, VehicleState};

use ados_protocol::hwcaps::is_rc_bridge_usb_id;
use framing::count_msp_frame_starts;
pub(crate) use framing::extract_frames;
use send_scheduler::STREAM_DEFAULT;
use transport::{
    fc_variant_for_port, is_candidate_port, now_iso, open_serial, open_udp_listen, parse_net_spec,
    probe_baud, same_device, split_serial, BoxedReadHalf, BoxedWriteHalf, NetSpec, ParamPersister,
    ProbeOutcome, UdpAdapter, BAUD_CANDIDATES, BAUD_FALLBACK,
};

/// Fixed interval between flight-controller reconnect attempts.
///
/// Flat, with no ceiling and no attempt cap. This is the sole
/// command-and-control path to the vehicle and there is no packaged fallback,
/// so a ladder that walked 1→2→4→8→16→30 s left a drone up to half a minute
/// with no FC link and no telemetry after a USB replug or a brownout, and an
/// operator staring at a dead vehicle with no explanation for why reconnection
/// was slow. The 3 s floor is what keeps a persistently unwritable port (one
/// that opens but never accepts a write, so the 1 Hz companion heartbeat fails
/// every time) from pinning the loop in a hot re-open cycle.
pub(crate) const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

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

/// How long a link opened without probe evidence (the fallback-baud open of a
/// port that stayed silent through the whole baud sweep) may stay open without
/// a HEARTBEAT or MSP traffic before the run loop closes it and sweeps again.
/// A silent UART never errors or reaches EOF, so without this the router would
/// sit on the wrong port for the life of the process.
const UNPROVEN_LINK_TIMEOUT: Duration = Duration::from_secs(HEARTBEAT_TIMEOUT.as_secs() * 3);

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

/// Cadence floor for the parameter-cache disk write while a `PARAM_REQUEST_LIST`
/// sweep is downloading. A sweep delivers hundreds of `PARAM_VALUE` frames back
/// to back and the cache is written whole-file, so a write per parameter would be
/// hundreds of rewrites of the same document. See
/// [`FcConnection::record_param`] for why a change OUTSIDE a sweep does not wait
/// for this.
const PARAM_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// The FC serial link plus its shared state. Cheap to wrap in an `Arc`; every
/// method takes `&self` and uses interior mutability so the run loop and the
/// periodic sender tasks share one connection.
/// The inter-arrival gap above which an ATTITUDE counts as late.
///
/// Chosen as the period of the slowest cadence a rate loop could be closed at
/// rather than as a target: at 20 Hz a gap past 50 ms means a frame was
/// genuinely skipped, not merely jittered. It bounds a COUNTER, so a value that
/// turns out to be wrong makes the count less interesting, never the aircraft
/// less safe.
const ATTITUDE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(50);

pub struct FcConnection {
    cfg: MavlinkConfig,
    state: std::sync::Arc<Mutex<VehicleState>>,
    params: std::sync::Arc<Mutex<ParamCache>>,
    /// Inbound MAVLink frame fan-out.
    ///
    /// Carries [`RxFrame`] (refcounted [`Bytes`] plus the host receive time),
    /// not `Vec<u8>`: one frame goes to the IPC socket task, the aux tee, the
    /// relayed-vehicle projection, the sensor lane and one task per connected
    /// GCS proxy client, so with a `Vec` the per-frame allocation count grew
    /// with the number of attached consumers. The bytes are unchanged — a frame
    /// is still forwarded verbatim.
    frame_tx: broadcast::Sender<RxFrame>,
    /// Raw FC->host byte lane. Populated only for an MSP FC (Betaflight/iNav),
    /// whose FC->host bytes are MSP responses, not MAVLink frames, so they never
    /// appear on `frame_tx` (extract_frames yields nothing). Empty for a MAVLink
    /// FC. The direct-GCS proxies subscribe to both lanes and forward whichever
    /// carries bytes, so a polling MSP GCS receives the FC's responses while the
    /// MAVLink frame path stays byte-unchanged.
    raw_tx: broadcast::Sender<Bytes>,
    // pub(crate): visible crate-wide only so a sibling module's tests (e.g.
    // aux_uplink_consumer's) can install a fake writer and assert what
    // send_bytes actually wrote, the same way connection::send_scheduler's
    // own tests already do from inside this module.
    pub(crate) writer: Mutex<Option<BoxedWriteHalf>>,
    /// Raised by a write/flush failure or the TX watchdog to ask the run loop
    /// to tear down the current link and re-open it (installing a fresh
    /// writer). Carries the link generation the failure happened on, so a
    /// failure recorded against a link that is already gone cannot tear down
    /// its healthy replacement. `None` until the first failure.
    reconnect: tokio::sync::watch::Sender<Option<u64>>,
    /// Generation of the installed writer, bumped under the writer lock each
    /// time the run loop installs one. See [`Self::reconnect`].
    link_gen: AtomicU64,
    /// Monotonic count of bytes successfully written to the flight controller.
    ///
    /// The quantity the TX watchdog takes deltas of. A live writer and an
    /// unreturned future are not evidence that bytes are moving; a counter that
    /// advances is, and the unconditional 1 Hz companion heartbeat is what makes
    /// "flat" unambiguous rather than merely quiet.
    tx_bytes: AtomicU64,
    /// Reference sample for the TX watchdog's rolling window.
    tx_liveness: Mutex<send_scheduler::TxLiveness>,
    seq: AtomicU8,
    /// FC system id learned from inbound heartbeats (default 1 = ArduPilot).
    target_system: AtomicU8,
    /// FC component id latched from the same heartbeat as `target_system`.
    target_component: AtomicU8,
    /// True once a real autopilot HEARTBEAT has latched `target_system` and
    /// `target_component` on this process. Until then the default 1/1 is a
    /// guess, and anything that must address the vehicle exactly (the fleet
    /// uplink filter, the swarm setpoint lane) refuses to act on it.
    fc_identity_latched: AtomicBool,
    // pub(crate): same reasoning as `writer` above — a sibling module's tests
    // need to mark a fake link live.
    pub(crate) connected: AtomicBool,
    /// Cumulative count of MAVLink frames decoded off the flight controller
    /// link (every checksum-valid frame the read loop extracts, plus the demo
    /// loop's synthetic frames). Published on the state snapshot as
    /// `fc_frames_decoded`; the supervisor judges this service's work on its
    /// deltas while the transport is open, because the process's own I/O keeps
    /// moving on timers even when the FC reader has wedged.
    fc_frames_decoded: AtomicU64,
    /// Cumulative client frames dropped because this node is a visor and they
    /// were flight actions (see [`visor_gate`]).
    visor_flight_frames_dropped: AtomicU64,
    /// When the last visor drop was logged, nanoseconds since `epoch` (0 never).
    visor_drop_logged_at: AtomicU64,
    /// Set by [`Self::open`] when the link it returned was opened at the
    /// fallback baud without any probe evidence (no HEARTBEAT, no MSP). Such a
    /// link is torn down by [`Self::run`] if it proves nothing within
    /// [`UNPROVEN_LINK_TIMEOUT`], so a silent port cannot hold the router.
    opened_unproven: AtomicBool,
    /// True once the current session saw a vehicle HEARTBEAT or MSP traffic.
    /// Reset at the start of every session.
    link_proven: AtomicBool,
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
    /// Monotonic nanoseconds since [`Self::epoch`] of the last inbound frame.
    /// An atomic rather than a mutex: it is stamped once per read on the
    /// hottest path and read by the stream cadence.
    last_msg_at: AtomicU64,
    /// The monotonic origin `last_msg_at` counts from.
    epoch: Instant,
    /// Whether the vehicle's most recent frame carried a MAVLink 2 signature:
    /// the FC answers signed, so it has a signing key and drops the unsigned
    /// frames this router originates on every link but channel 0.
    fc_signed: AtomicBool,
    /// The canonical device path of the open serial FC link (symlinks such as
    /// `/dev/serial0` resolved). `None` for a network source or no link.
    fc_device: Mutex<Option<String>>,
    /// How evenly ATTITUDE arrives from the flight controller.
    ///
    /// ATTITUDE specifically because it is the fastest thing the autopilot
    /// volunteers and the one a rate loop would have to close against, so its
    /// cadence -- not the aggregate message rate -- is what decides whether a
    /// loop over this link is viable. Measured always rather than behind a
    /// flag: the numbers are needed before anything is built on them, and a
    /// measurement taken only once someone remembers to ask has no history.
    attitude_cadence: Mutex<crate::cadence::InboundCadence>,
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
    /// Param cache size as of the last time the sweep checked whether it was
    /// due to send. Lets `tick_param_sweep` tell "genuinely stalled" apart
    /// from "a client's own PARAM_REQUEST_LIST is actively feeding the
    /// cache" — see the comment at its use site.
    param_last_cached_count: AtomicUsize,
    /// The ground station's aux-uplink sender, when this node is relaying a
    /// linked drone rather than driving a local flight controller. `None` on
    /// a drone (which always has, or is trying to reach, a real FC writer)
    /// and on a ground station before the receive chain has come up. See
    /// [`Self::send_bytes`] for how this becomes the fallback destination for
    /// a client's outbound MAVLink.
    aux_uplink: Mutex<Option<crate::aux_uplink::AuxUplinkSender>>,
    /// The PIC-arbiter injector gate, built only where the arbiter runs (see
    /// `MavlinkConfig::injector_arbitration_active`); `None` means declared
    /// injectors are not gated on this node. A `std::sync::Mutex` held only
    /// for the synchronous verdict, which does no file I/O.
    injector_gate: Option<std::sync::Mutex<injector_gate::InjectorGateCache>>,
    /// Background reader of the PIC sidecar and pairing file the gate and the
    /// swarm loop consume. Started by [`Self::run_gate_feed`].
    gate_feed: std::sync::Arc<injector_gate::GateFeed>,
    /// The single writer the parameter cache is persisted through, newest
    /// snapshot wins.
    param_persister: ParamPersister,
    /// Route-aware forwarding state (`mavlink.route_aware`); inert when off.
    routes: std::sync::Arc<crate::routing::RouteHub>,
    /// Agent-held signing of this router's own frames (`mavlink.agent_signing`).
    agent_signing: crate::signing::AgentSigning,
    /// When the firmware banner was last requested (see
    /// `tick_banner_request`); `None` before the first request.
    last_banner_req: Mutex<Option<Instant>>,
    /// Capture marker and IMU-stream fallback state the stream cadence reads.
    sensor_streams: imu_fallback::SensorStreams,
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
        let gate_feed = std::sync::Arc::new(injector_gate::GateFeed::new());
        let injector_gate = cfg
            .injector_arbitration_active()
            .then(|| std::sync::Mutex::new(injector_gate::InjectorGateCache::new(&gate_feed)));
        let routes = crate::routing::RouteHub::new(cfg.route_aware);
        let agent_signing = crate::signing::AgentSigning::from_config(cfg.agent_signing);
        std::sync::Arc::new(Self {
            cfg,
            state,
            params,
            frame_tx,
            raw_tx,
            writer: Mutex::new(None),
            reconnect: tokio::sync::watch::channel(None).0,
            link_gen: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_liveness: Mutex::new(send_scheduler::TxLiveness::new()),
            seq: AtomicU8::new(0),
            target_system: AtomicU8::new(1),
            target_component: AtomicU8::new(1),
            fc_identity_latched: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            fc_frames_decoded: AtomicU64::new(0),
            visor_flight_frames_dropped: AtomicU64::new(0),
            visor_drop_logged_at: AtomicU64::new(0),
            opened_unproven: AtomicBool::new(false),
            link_proven: AtomicBool::new(false),
            open_failed: AtomicBool::new(false),
            port: Mutex::new(String::new()),
            fc_variant: Mutex::new(None),
            baud: AtomicU32::new(0),
            last_msg_at: AtomicU64::new(0),
            epoch: Instant::now(),
            fc_signed: AtomicBool::new(false),
            fc_device: Mutex::new(None),
            attitude_cadence: Mutex::new(crate::cadence::InboundCadence::new(ATTITUDE_DEADLINE)),
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
            param_last_cached_count: AtomicUsize::new(0),
            aux_uplink: Mutex::new(None),
            injector_gate,
            gate_feed,
            param_persister: ParamPersister::new(),
            routes,
            agent_signing,
            last_banner_req: Mutex::new(None),
            sensor_streams: imu_fallback::SensorStreams::default(),
        })
    }

    /// Subscribe to the raw inbound frame stream (the MAVLink socket + proxies).
    pub fn subscribe(&self) -> broadcast::Receiver<RxFrame> {
        self.frame_tx.subscribe()
    }

    /// The route-aware forwarding hub.
    pub fn routes(&self) -> &std::sync::Arc<crate::routing::RouteHub> {
        &self.routes
    }

    /// Capture marker and IMU-stream fallback state (see [`imu_fallback`]).
    pub fn sensor_streams(&self) -> &imu_fallback::SensorStreams {
        &self.sensor_streams
    }

    /// The resolved agent profile this router runs under.
    pub fn profile(&self) -> &str {
        &self.cfg.profile
    }

    /// The FC frame source for the consumer of endpoint `labels[0]` (see
    /// [`crate::routing::RouteHub::feed`]): the flat fan-out when route-aware
    /// forwarding is off.
    pub fn frame_feed(&self, labels: &[&str]) -> crate::routing::FrameFeed {
        self.routes.feed(labels, self.subscribe())
    }

    /// The agent-held signing state.
    pub fn agent_signing(&self) -> &crate::signing::AgentSigning {
        &self.agent_signing
    }

    /// Subscribe to the raw inbound FC byte lane (populated only for an MSP FC).
    pub fn subscribe_raw(&self) -> broadcast::Receiver<Bytes> {
        self.raw_tx.subscribe()
    }

    /// Publish a frame that arrived from off-board onto the fan-out, as if it
    /// had come off a local flight controller.
    ///
    /// This exists for a node that has no flight controller of its own but does
    /// receive a vehicle's frames some other way — a ground station decoding
    /// them off the radio's auxiliary lane. Publishing here is what makes every
    /// existing consumer work unchanged: the IPC socket, the TCP and UDP
    /// transports, and the WebSocket transport all read this one fan-out, so a
    /// ground control station connected to such a node sees the vehicle exactly
    /// as it would on a direct link.
    ///
    /// **It publishes and nothing else.** It deliberately does not stamp the
    /// heartbeat clock, set the connected flag, learn a target system id, or
    /// update the vehicle state, all of which the local read loop does for a
    /// frame off a real link. Doing any of that here would make a node with no
    /// flight controller report that it has one — the link surfaces would read
    /// "connected" while `fc_port` named nothing, and an operator would be told
    /// a vehicle is attached to the wrong box. Relaying someone else's frames
    /// is not the same claim as having a flight controller, and this seam keeps
    /// the two apart.
    ///
    /// Returns whether any consumer was subscribed. `false` means the frame was
    /// dropped for want of a listener, which is a normal idle state (no ground
    /// control station connected) and not an error. The frame is stamped with
    /// the current host monotonic time, its receive time on this node.
    pub fn inject_frame(&self, frame: Bytes) -> bool {
        self.frame_tx.send(RxFrame::now(frame)).is_ok()
    }

    /// Whether the FC transport is open: the serial node / network socket has
    /// been opened and not yet torn down. This is NOT proof the FC is talking —
    /// see [`Self::mavlink_alive`]. Kept distinct so a consumer can render
    /// "port open, no MAVLink" separately from "no port".
    pub fn transport_open(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Cumulative MAVLink frames decoded off the flight-controller link since
    /// the process started. Never reset, so a consumer takes deltas.
    pub fn frames_decoded(&self) -> u64 {
        self.fc_frames_decoded.load(Ordering::Relaxed)
    }
    /// Seconds since the last decoded HEARTBEAT, or `None` when none has been
    /// seen on the current process. The freshness signal the alive gate reads.
    /// The current attitude-arrival cadence.
    ///
    /// Exposed so the measurement can be read by whatever is asking -- a
    /// periodic emitter, a diagnostic, or a test -- rather than only existing
    /// inside the read loop that produces it.
    pub async fn attitude_cadence(&self) -> crate::cadence::CadenceSnapshot {
        self.attitude_cadence.lock().await.snapshot()
    }

    /// Forget the attitude timing history, keeping the cumulative counters.
    ///
    /// Called when the link drops so the gap spanning an outage does not land
    /// in a distribution that otherwise describes milliseconds.
    pub async fn reset_attitude_cadence(&self) {
        self.attitude_cadence.lock().await.reset_stream();
    }
}

mod discovery;
mod reader;
#[cfg(test)]
use discovery::*;
mod status;

#[cfg(test)]
mod crsf_exclusion_tests;

#[cfg(test)]
mod command_gate_tests;

#[cfg(test)]
mod liveness_tests;

#[cfg(test)]
mod passthrough_tests;
