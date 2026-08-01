//! `ados-mavlink-router` binary.
//!
//! Owns the FC serial link and serves the MAVLink + state IPC sockets plus the
//! direct-GCS TCP/UDP proxies. Mirrors the Python `ados-mavlink` service
//! (`python -m ados.services.mavlink`): the IPC servers, the FC connection, the
//! 1 Hz companion heartbeat, the 10 Hz state publish, the adaptive stream
//! cadence, and the parameter sweep. The state socket is published as
//! length-prefixed msgpack (v2), the versioned wire the shared reader
//! auto-detects.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::state::encode_v2;
use serde_json::{json, Map, Value};
use tokio::sync::{Mutex, Notify};

use ados_mavlink_router::aux_rpc_dedupe::RequestDedupe;
use ados_mavlink_router::aux_tee::{self, TeeCounters};
use ados_mavlink_router::aux_uplink;
use ados_mavlink_router::aux_uplink_consumer::{self, AuxUplinkConsumerCounters};
use ados_mavlink_router::config::MavlinkConfig;
use ados_mavlink_router::connection::swarm_setpoint::{self, SwarmSetpointStatus};
use ados_mavlink_router::connection::attitude_setpoint::{self, AttitudeSetpointStatus};
use ados_mavlink_router::connection::ClientOrigin;
use ados_mavlink_router::connection::FcConnection;
use ados_mavlink_router::frame_ingest::{self, IngestCounters, INGEST_QUEUE_DEPTH};
use ados_mavlink_router::param_cache::ParamCache;
use ados_mavlink_router::proxies::{
    proxy_bind_addr, run_tcp_proxy, run_udp_proxy, run_ws_proxy, ProxyAuth, WsProxyAuth,
};
use ados_mavlink_router::relayed::RelayedVehicle;
use ados_mavlink_router::state::{firmware_family, VehicleState};
use ados_swarm_control::ModePrecedence;

const MAVLINK_QUEUE_DEPTH: usize = 256;
const STATE_QUEUE_DEPTH: usize = 32;
const TCP_PROXY_PORT: u16 = 5760;
const UDP_PROXY_PORTS: &[u16] = &[14550, 14551];
/// The auxiliary lane's loopback port pair, resolved from operator config.
///
/// These used to be matching literals here, in the control surface and in the
/// radio service, with comments pointing at each other. That is correct at
/// exactly one value: the ports are operator-settable, and changing one moved
/// the radio while the other two kept writing to the old number, so the uplink
/// carried nothing and nothing reported an error. Resolving from the shared
/// reader moves all three together.
fn aux_ports() -> ados_protocol::aux_ports::AuxPorts {
    ados_protocol::aux_ports::AuxPorts::load()
}

fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// This node's own device id file, mirroring `ados-control`'s
/// `config::DEVICE_ID_FILE`. Defined locally rather than depending on
/// `ados-control`: the router must not pull the whole HTTP control surface in
/// to read one path.
const DEVICE_ID_FILE: &str = "/etc/ados/device-id";

/// Resolve this drone's device id, so a relay-proxy request addressed to a
/// peer is only answered by the peer it names.
///
/// Same precedence as `ados-control`: the provisioned file wins, then
/// `ADOS_DEVICE_ID`, then empty. Empty means every request is accepted, which
/// preserves the single-drone behaviour on a box that has no id yet.
fn own_device_id() -> Arc<str> {
    let from_file = std::fs::read_to_string(DEVICE_ID_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let resolved = from_file
        .or_else(|| {
            std::env::var("ADOS_DEVICE_ID")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    Arc::from(resolved.as_str())
}

/// Demo mode: drive synthetic telemetry instead of opening a serial FC. Enabled
/// by the `--demo` argument or `ADOS_MAVLINK_DEMO=1`. Off by default, so the
/// production unit (no argument, no env) keeps the serial path.
fn demo_enabled() -> bool {
    std::env::args().any(|a| a == "--demo")
        || std::env::var("ADOS_MAVLINK_DEMO").ok().as_deref() == Some("1")
}

/// TCP proxy bind port. Overridable via `ADOS_MAVLINK_TCP_PORT` (the parity
/// harness uses this to run a second instance without a port clash); defaults to
/// the standard port.
fn tcp_proxy_port() -> u16 {
    std::env::var("ADOS_MAVLINK_TCP_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(TCP_PROXY_PORT)
}

/// UDP proxy bind ports (comma-separated). Overridable via
/// `ADOS_MAVLINK_UDP_PORTS`; defaults to the standard ports. An empty or
/// unparseable override falls back to the defaults.
fn udp_proxy_ports() -> Vec<u16> {
    let parsed: Vec<u16> = std::env::var("ADOS_MAVLINK_UDP_PORTS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|p| p.trim().parse::<u16>().ok())
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        UDP_PROXY_PORTS.to_vec()
    } else {
        parsed
    }
}

/// WebSocket proxy bind port. `ADOS_MAVLINK_WS_PORT` overrides the configured
/// endpoint port when set (used by the parity harness); otherwise the first
/// enabled WebSocket endpoint from the config selects it.
fn ws_proxy_port(cfg: &MavlinkConfig) -> Option<u16> {
    if let Ok(v) = std::env::var("ADOS_MAVLINK_WS_PORT") {
        return v.trim().parse().ok();
    }
    cfg.websocket_port()
}
/// How often the attitude-cadence distribution is shipped to the store.
///
/// The window is already a summary by this point, so a faster cadence would add
/// writes without adding information, and this measurement must not disturb the
/// thing it is measuring.
const CADENCE_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;

    // fmt as the primary sink (this binary has no journald layer) plus the logd
    // layer that ships records to the logging daemon's ingest socket; the logd
    // layer is best-effort and never blocks the service.
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-mavlink-router"))
        .try_init();
    tracing::info!("mavlink_router_starting");

    let cfg = MavlinkConfig::load();

    let state = Arc::new(Mutex::new(VehicleState::default()));
    let mut pc = ParamCache::default_path();
    if let Err(e) = pc.load() {
        tracing::warn!(error = %e, "param_cache_load_failed");
    }
    let params = Arc::new(Mutex::new(pc));

    let fc = FcConnection::new(cfg.clone(), state.clone(), params.clone());
    let cancel = Arc::new(Notify::new());

    let dir = run_dir();
    let mavlink_sock = format!("{dir}/mavlink.sock");
    let msp_sock = format!("{dir}/msp.sock");
    let state_sock = format!("{dir}/state.sock");

    // MAVLink socket: fan FC frames out (256-deep), accept client commands inbound.
    let (mavlink_ipc, inbound) = match IpcBroadcast::bind(
        &mavlink_sock,
        MAVLINK_QUEUE_DEPTH,
        false,
        Some(MAVLINK_QUEUE_DEPTH),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %mavlink_sock, error = %e, "mavlink_sock_bind_failed");
            return;
        }
    };
    let mavlink_ipc = Arc::new(mavlink_ipc);
    let mut inbound = inbound.expect("inbound channel requested");

    // MSP socket: the sibling byte plane for an MSP FC (Betaflight/iNav), whose
    // FC->host bytes are raw MSP responses rather than MAVLink frames. The MAVLink
    // socket is fed only by the parsed frame lane, so it stays legitimately silent
    // for such an FC; this socket carries the raw byte lane instead so a downstream
    // consumer (the cloud relay) reaches a polling MSP GCS the same way it reaches
    // a MAVLink one. Length-prefixed both ways (256-deep), accepting protocol-
    // agnostic client commands inbound. Never parses the byte stream.
    let (msp_ipc, msp_inbound) = match IpcBroadcast::bind(
        &msp_sock,
        MAVLINK_QUEUE_DEPTH,
        false,
        Some(MAVLINK_QUEUE_DEPTH),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %msp_sock, error = %e, "msp_sock_bind_failed");
            return;
        }
    };
    let msp_ipc = Arc::new(msp_ipc);
    let mut msp_inbound = msp_inbound.expect("msp inbound channel requested");

    // State socket: replay last snapshot on connect (32-deep), no inbound.
    let (state_ipc, _) = match IpcBroadcast::bind(&state_sock, STATE_QUEUE_DEPTH, true, None).await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(path = %state_sock, error = %e, "state_sock_bind_failed");
            return;
        }
    };
    let state_ipc = Arc::new(state_ipc);

    let started = Instant::now();
    let mut tasks = Vec::new();
    // Set when the aux MAVLink tee runs, so its counters ride the state
    // snapshot. Absent on a profile that does not run the tee, which reads as
    // "not running" rather than as a lane that forwarded nothing.
    let mut aux_tee_counters: Option<Arc<TeeCounters>> = None;
    // Set when the republish seam runs, on the same "absent means not running"
    // reading as the tee counters above.
    let mut frame_ingest_counters: Option<Arc<IngestCounters>> = None;
    // The decoded reading of the vehicle on the far end of the radio, set on
    // the same profiles as the seam that feeds it. Kept strictly apart from the
    // attached-FC `state` above: this node still has no flight controller of
    // its own, it can now simply see the one it is relaying.
    let mut relayed_vehicle: Option<Arc<StdMutex<RelayedVehicle>>> = None;
    // Set when the relay-proxy uplink runs, on the same reading as the two
    // above: absent means the lane is not running on this profile, which is not
    // the same signal as a lane that received nothing.
    let mut aux_rpc_counters: Option<AuxUplinkConsumerCounters> = None;

    // FC connect + read loop. In demo mode a synthetic source feeds the same
    // fan-out, state, and proxy paths a serial FC would; the serial path is
    // untouched when demo mode is off (the default).
    let demo = demo_enabled();
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        if demo {
            tracing::info!("mavlink_router_demo_mode");
            tasks.push(tokio::spawn(async move { fc.run_demo(cancel).await }));
        } else {
            tasks.push(tokio::spawn(async move { fc.run(cancel).await }));
        }
    }

    // Ship the attitude-cadence distribution to the logging store.
    //
    // The measurement lives in memory in the read loop, which answers the
    // question only for whoever is holding the process open at the time. A
    // decision about closing a control loop over this link needs the tail
    // across a whole bench run, and needs it to survive the run -- so the
    // percentiles go where they can be queried afterwards rather than only
    // observed live.
    //
    // Every ten seconds, not every arrival: the samples are already summarised
    // by then, and emitting per-message would put a few hundred writes a second
    // onto a store this measurement is not supposed to disturb.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let metrics = ados_protocol::logd::emitter::IngestEmitter::new("ados-mavlink");
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(CADENCE_REPORT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let s = fc.attitude_cadence().await;
                        // Nothing to say before the first gap exists. An empty
                        // window emitting zeros would put a fabricated 0 Hz into
                        // the series and make a link that has not started look
                        // like one that stopped.
                        let (Some(hz), Some(p50), Some(p95), Some(p99)) =
                            (s.achieved_hz, s.p50, s.p95, s.p99)
                        else {
                            continue;
                        };
                        use ados_protocol::logd::{Fields, Value};
                        let mut tags = Fields::new();
                        tags.insert("message".to_string(), Value::from("attitude"));
                        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
                        metrics.emit_metric("fc.attitude.achieved_hz", hz, tags.clone());
                        metrics.emit_metric("fc.attitude.period_p50_ms", ms(p50), tags.clone());
                        metrics.emit_metric("fc.attitude.period_p95_ms", ms(p95), tags.clone());
                        metrics.emit_metric("fc.attitude.period_p99_ms", ms(p99), tags.clone());
                        metrics.emit_metric(
                            "fc.attitude.deadline_misses",
                            s.missed_deadline as f64,
                            tags,
                        );
                    }
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // 1 Hz companion heartbeat.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => fc.send_heartbeat().await,
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // Adaptive stream cadence + parameter sweep (evaluated every second).
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        fc.tick_streams().await;
                        fc.tick_param_sweep().await;
                    }
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // FC frames -> MAVLink socket clients. The socket contract is 4-byte
    // big-endian length-prefixed in both directions (the inbound reader decodes
    // the prefix), so each raw FC frame is framed before it is broadcast. The
    // proxies consume the raw frame stream directly and are unaffected.
    {
        let mavlink_ipc = mavlink_ipc.clone();
        let cancel = cancel.clone();
        let mut rx = fc.subscribe();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = rx.recv() => match frame {
                        Ok(f) => match encode_frame(&f, MAVLINK_MAX_FRAME) {
                            Ok(framed) => mavlink_ipc.broadcast(framed).await,
                            Err(e) => tracing::warn!(error = %e, "mavlink_frame_encode_failed"),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // FC frames -> the radio's auxiliary lane, so a ground station linked over
    // the radio gets the vehicle's MAVLink without a shared network. Drone
    // profile only: a ground station is the receiving end of this lane, and its
    // own radio service does not serve the aux transmit pair at all.
    if cfg.is_drone() {
        let counters = Arc::new(TeeCounters::default());
        aux_tee_counters = Some(counters.clone());
        // Another consumer of the same fan-out the IPC socket reads. A slow aux
        // lane makes this receiver lag and shed, never the producer stall.
        let frames = fc.subscribe();
        let egress = ados_protocol::aux_egress::AuxEgress::new(format!("{dir}/radio-aux.sock"));
        let tee_cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            aux_tee::run(
                frames,
                egress,
                counters,
                aux_tee::ShaperConfig::default(),
                tee_cancel,
            )
            .await
        }));

        // The other half of the same pair: a ground station's outbound
        // MAVLink (a client's arm/mode/param/mission command, relayed over
        // the radio) arrives decoded on this loopback — the drone's own
        // `wfb_rx -p3` has always run and re-emitted here, but until now
        // nothing read the port, so it landed in a void. Injecting into the
        // FC via `send_bytes` is what actually closes the ground-to-drone
        // half of the relay.
        let uplink_counters = AuxUplinkConsumerCounters::new();
        aux_rpc_counters = Some(uplink_counters.clone());
        // One dedupe cache for the whole process. The ground retransmits a
        // Request datagram it has no answer for — the only way to survive an
        // uplink that loses a fifth of its packets to the video TX burst — so
        // without this a retried PUT would execute twice. Shared by every
        // spawned handler, hence the Arc.
        let uplink_dedupe = Arc::new(RequestDedupe::new());
        let uplink_fc = fc.clone();
        let uplink_cancel = cancel.clone();
        // The drone's own AuxEgress for sending RPC responses back over the
        // aux downlink (radio_id 2). Same command socket the aux_tee uses.
        let uplink_egress = Arc::new(ados_protocol::aux_egress::AuxEgress::new(
            std::path::Path::new(&format!("{dir}/radio-aux.sock")),
        ));
        let uplink_device_id = own_device_id();
        if uplink_device_id.is_empty() {
            tracing::warn!(
                path = DEVICE_ID_FILE,
                "aux_rpc_own_device_id_unresolved_accepting_every_target"
            );
        } else {
            tracing::info!(device_id = %uplink_device_id, "aux_rpc_own_device_id_resolved");
        }
        tasks.push(tokio::spawn(async move {
            aux_uplink_consumer::run(
                aux_ports().rx,
                uplink_fc,
                Some(uplink_egress),
                uplink_device_id,
                uplink_dedupe,
                uplink_counters,
                uplink_cancel,
            )
            .await
        }));
    } else {
        tracing::info!(
            profile = %cfg.profile,
            "mavlink_aux_tee_skipped_for_profile"
        );
    }

    // The mirror of the tee, on the receiving rig. A ground station has no
    // flight controller of its own; the vehicle's frames arrive over the radio,
    // are decoded by the ground data plane, and enter here. Publishing them to
    // the fan-out is what makes the transports this router already serves carry
    // the vehicle, so a ground control station connected to the ground station
    // sees it over the ports it already uses. Ground-station profile only: a
    // drone has its own flight controller and must never take frames from
    // off-board as if they were its own.
    if cfg.is_ground_station() {
        let ingest_sock = format!(
            "{dir}/{}",
            ados_protocol::mavlink_ingest::MAVLINK_INGEST_SOCK_NAME
        );
        // Bound only as an inbound frame reader. The broadcast direction is
        // deliberately unused: this socket exists to carry frames INTO the
        // fan-out, and a consumer wanting frames back out has the MAVLink
        // socket and the transports already.
        match IpcBroadcast::bind(
            &ingest_sock,
            INGEST_QUEUE_DEPTH,
            false,
            Some(INGEST_QUEUE_DEPTH),
        )
        .await
        {
            Ok((server, inbound)) => {
                let inbound = inbound.expect("inbound channel requested");
                let counters = Arc::new(IngestCounters::default());
                frame_ingest_counters = Some(counters.clone());
                let relayed = Arc::new(StdMutex::new(RelayedVehicle::default()));
                relayed_vehicle = Some(relayed.clone());
                // Held for the process lifetime: dropping the server closes the
                // socket and the ground data plane would find nothing to
                // connect to.
                let server = Arc::new(server);
                let fc = fc.clone();
                let cancel = cancel.clone();
                tasks.push(tokio::spawn(async move {
                    let _server = server;
                    frame_ingest::run(inbound, fc, counters, relayed, cancel).await
                }));
            }
            Err(e) => {
                // Not fatal: without this seam the ground station still serves
                // its own surfaces, it just cannot relay a vehicle. Say so
                // loudly rather than starting up looking healthy.
                tracing::error!(
                    path = %ingest_sock,
                    error = %e,
                    "mavlink_frame_ingest_bind_failed"
                );
            }
        }

        // The other half of the same relay: a connected client's outbound
        // MAVLink (arm, mode, param read/write, mission commands, ...) used
        // to reach this ground station and go no further — `send_bytes`
        // wrote to a local FC that does not exist, and silently dropped
        // everything. This installs the fallback `send_client_bytes` uses:
        // frame + batch + radiate on the aux uplink (radio_id 3) toward
        // whichever drone the ground station's WFB link has bound, closing
        // the ground-to-drone half of the relay (the drone-to-ground half
        // has run since the aux downlink lane was wired up).
        fc.set_aux_uplink(aux_uplink::spawn(aux_ports().tx)).await;
    }

    // MAVLink socket client commands -> FC.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = inbound.recv() => match cmd {
                        // The on-box IPC socket is not reachable off the node,
                        // so a caller here has already crossed a boundary the
                        // raw network sockets do not have — unless the caller
                        // is forwarding bytes that reached the node some other
                        // way, which it now says so the reading is its own
                        // rather than the socket's.
                        Some(cmd) => {
                            let origin = ClientOrigin::from_ipc_peer(&cmd.peer);
                            fc.send_client_bytes(&cmd.payload, origin).await
                        }
                        None => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // FC raw MSP bytes -> MSP socket clients. An MSP FC's FC->host bytes travel
    // the raw byte lane (the frame lane stays silent for it), so length-prefix each
    // chunk exactly like the MAVLink socket and fan it to the MSP clients. A ≤2 KB
    // MSP response fits the 64 KB frame cap; the byte stream is never parsed here
    // (transparent passthrough). The `RecvError::Lagged`/`Closed` handling mirrors
    // the MAVLink frame producer above.
    {
        let msp_ipc = msp_ipc.clone();
        let cancel = cancel.clone();
        let mut raw_rx = fc.subscribe_raw();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    chunk = raw_rx.recv() => match chunk {
                        Ok(bytes) => match encode_frame(&bytes, MAVLINK_MAX_FRAME) {
                            Ok(framed) => msp_ipc.broadcast(framed).await,
                            Err(e) => tracing::warn!(error = %e, "msp_chunk_encode_failed"),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // MSP socket client commands -> FC. The GCS->FC path is protocol-agnostic, so
    // the received bytes are written verbatim to the FC (no MSP parse), identical
    // to the MAVLink socket's inbound path.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = msp_inbound.recv() => match cmd {
                        Some(cmd) => {
                            let origin = ClientOrigin::from_ipc_peer(&cmd.peer);
                            fc.send_client_raw(&cmd.payload, origin).await
                        }
                        None => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // 10 Hz state publish: vehicle snapshot + the service runtime extras.
    // Onboard swarm autonomy. The loop reads the swarm neighbour table off
    // `/run/ados/swarm.sock`, runs the control laws at 10 Hz and commands the FC
    // through this router's own send path. `SwarmSetpointStatus` is the reverse
    // direction: the active precedence level and the emergency condition, which the
    // state snapshot below republishes for `ados-swarmbus` to fold into the beacon.
    // Returns immediately when the swarm is disabled or no drone slot is assigned,
    // so an operator who has not turned it on pays for no socket and no timer.
    let swarm_status: Option<Arc<SwarmSetpointStatus>> = {
        let status = Arc::new(SwarmSetpointStatus::default());
        let fc = fc.clone();
        let state = state.clone();
        let swarm_sock = format!("{dir}/swarm.sock");
        let handle = status.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            swarm_setpoint::run(
                fc,
                state,
                swarm_sock,
                ados_mavlink_router::config::CONFIG_YAML.to_string(),
                handle,
                cancel,
            )
            .await
        }));
        Some(status)
    };

    // The attitude rung: body-rate/thrust `SET_ATTITUDE_TARGET` out, a second
    // way to fly ArduPilot until the G3 gate (a real Betaflight FC) passes.
    // Gated INERT here: no rate injector producer or config enables it yet, so
    // it never emits a live attitude command to any airframe (the G3 test is
    // written failing-first and left #[ignore]d). `AttitudeSetpointStatus` is
    // the reverse direction: the lane's verdict + counters for the snapshot.
    let attitude_status: Option<Arc<AttitudeSetpointStatus>> = {
        let status = Arc::new(AttitudeSetpointStatus::default());
        let fc = fc.clone();
        let state = state.clone();
        let pic_path = format!("{dir}/pic-state.json");
        let handle = status.clone();
        let cancel = cancel.clone();
        // A watch channel carrying the newest live rate command + its attested
        // identity. No producer is wired yet (that is the G3-gated lane), so
        // the channel stays empty and the rung suppresses to the human hold.
        let (rate_tx, rate_rx) =
            tokio::sync::watch::channel::<Option<(ados_rate_control::AttitudeCommand, String, std::time::Instant)>>(None);
        let _keep_writer_alive = rate_tx;
        tasks.push(tokio::spawn(async move {
            attitude_setpoint::run(
                fc,
                state,
                false, // inert until a producer + config are wired
                pic_path,
                rate_rx,
                handle,
                cancel,
            )
            .await
        }));
        Some(status)
    };

    {
        let fc = fc.clone();
        let state = state.clone();
        let params = params.clone();
        let state_ipc = state_ipc.clone();
        let swarm_status = swarm_status.clone();
        let mavlink_ipc_stats = mavlink_ipc.clone();
        let aux_tee_counters = aux_tee_counters.clone();
        let aux_rpc_counters = aux_rpc_counters.clone();
        let frame_ingest_counters = frame_ingest_counters.clone();
        let relayed_vehicle = relayed_vehicle.clone();
        let attitude_status = attitude_status.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            // Last reported eviction counts, so a fresh eviction logs once
            // rather than every 100 ms tick while the count sits unchanged.
            let mut last_mavlink_drops = 0u64;
            let mut last_state_drops = 0u64;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let mavlink_drops = mavlink_ipc_stats.dropped_clients();
                        let state_drops = state_ipc.dropped_clients();
                        // Surface a newly evicted slow consumer as a log line
                        // (also shipped to the logging daemon) so the eviction
                        // is not silent. The continuous signal rides the state
                        // snapshot below for the GCS.
                        if mavlink_drops > last_mavlink_drops {
                            tracing::warn!(
                                socket = "mavlink",
                                evicted = mavlink_drops - last_mavlink_drops,
                                total = mavlink_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_mavlink_drops = mavlink_drops;
                        }
                        if state_drops > last_state_drops {
                            tracing::warn!(
                                socket = "state",
                                evicted = state_drops - last_state_drops,
                                total = state_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_state_drops = state_drops;
                        }
                        let extras = build_extras(
                            &fc, &state, &params, started, mavlink_drops, state_drops,
                            aux_tee_counters.as_ref(), frame_ingest_counters.as_ref(),
                            aux_rpc_counters.as_ref(), swarm_status.as_ref(),
                            attitude_status.as_ref(),
                            relayed_vehicle.as_ref(),
                        )
                        .await;
                        let wire = { state.lock().await.to_wire_with(&extras) };
                        let encoded = encode_v2(&wire);
                        match encoded {
                            Ok(bytes) => state_ipc.broadcast(bytes).await,
                            Err(e) => tracing::warn!(error = %e, "state_encode_failed"),
                        }
                    }
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // Direct-GCS proxies. Bind ports default to the standard values and are
    // overridable via env so a second instance (the parity harness) can run
    // alongside the first without a port clash.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let port = tcp_proxy_port();
        let bind = proxy_bind_addr();
        // Same posture object the WebSocket uses. Observe-only here regardless
        // of the flag for now: this port is advertised to operators as the
        // QGroundControl / Mission Planner path, so it logs what it would have
        // refused rather than refusing it.
        let auth = ProxyAuth::from_config(false);
        tasks.push(tokio::spawn(async move {
            run_tcp_proxy(fc, &bind, port, auth, cancel).await
        }));
    }
    for port in udp_proxy_ports() {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let auth = ProxyAuth::from_config(false);
        let bind = proxy_bind_addr();
        tasks.push(tokio::spawn(async move {
            run_udp_proxy(fc, &bind, port, auth, cancel).await
        }));
    }
    if let Some(ws_port) = ws_proxy_port(&cfg) {
        let fc = fc.clone();
        let cancel = cancel.clone();
        // The direct WebSocket proxy carries raw MAVLink to/from the FC, so a
        // paired agent gates an off-box connection on the stored pairing key.
        // Enforcement is config-driven and defaults off (observe-only), so this
        // build does not change the data path until a bench session enables it.
        let auth = WsProxyAuth::from_config(cfg.ws_proxy_enforce_auth);
        tasks.push(tokio::spawn(async move {
            run_ws_proxy(fc, ws_port, auth, cancel).await
        }));
    }

    tracing::info!("mavlink_router_ready");
    wait_for_shutdown().await;
    tracing::info!("mavlink_router_stopping");
    cancel.notify_waiters();
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("mavlink_router_stopped");
}

/// Build the runtime extras the state snapshot carries on top of the vehicle
/// fields.
///
/// `mavlink_drops` / `state_drops` are the monotonic slow-consumer eviction
/// counts from the two IPC servers, carried on the snapshot so a silently
/// pruned client is observable to Mission Control (it never sees an error of
/// its own when it is dropped for falling behind).
/// The honest connected-or-reachable verdict, from the same four inputs
/// `/api/status` and `/api/status/full` already gate on. True for a live
/// MAVLink heartbeat, and also true for a healthy MSP flight controller
/// (Betaflight/iNav) with its transport open — an MSP board never emits a
/// MAVLink heartbeat, so it would otherwise read as absent despite being
/// reachable and drivable over the byte-transparent proxy.
fn compute_fc_reachable(
    mavlink_alive: bool,
    transport_open: bool,
    has_fc_variant: bool,
    fc_link_hint: &str,
) -> bool {
    mavlink_alive || (transport_open && (has_fc_variant || fc_link_hint == "msp_detected"))
}

#[allow(clippy::too_many_arguments)]
async fn build_extras(
    fc: &Arc<FcConnection>,
    state: &Arc<Mutex<VehicleState>>,
    params: &Arc<Mutex<ParamCache>>,
    started: Instant,
    mavlink_drops: u64,
    state_drops: u64,
    aux_tee_counters: Option<&Arc<TeeCounters>>,
    frame_ingest_counters: Option<&Arc<IngestCounters>>,
    aux_rpc_counters: Option<&AuxUplinkConsumerCounters>,
    swarm: Option<&Arc<SwarmSetpointStatus>>,
    attitude: Option<&Arc<AttitudeSetpointStatus>>,
    relayed_vehicle: Option<&Arc<StdMutex<RelayedVehicle>>>,
) -> Map<String, Value> {
    // The cached param count and the map's change counter, read under one lock.
    let (cached, param_generation) = {
        let pc = params.lock().await;
        (pc.count(), pc.generation())
    };
    // The expected param count and the decoded autopilot code, read under one
    // lock. `autopilot` is the already-decoded HEARTBEAT discriminator (3 =
    // ArduPilot, 12 = PX4) used to name the MAVLink firmware family below.
    let (expected, autopilot) = {
        let s = state.lock().await;
        (s.param_count, s.autopilot)
    };
    let mut extras = Map::new();
    // The gated truth: fc_connected = transport_open && mavlink_alive. A port
    // that opens but never hears a HEARTBEAT reads transport_open:true but
    // fc_connected:false, so a broken link never shows "connected". Surface the
    // two halves + the heartbeat age + the configured source so a consumer can
    // render "port open · no MAVLink" distinctly and validate the link is live.
    let transport_open = fc.transport_open();
    let mavlink_alive = fc.mavlink_alive().await;
    extras.insert(
        "fc_connected".into(),
        json!(transport_open && mavlink_alive),
    );
    extras.insert("transport_open".into(), json!(transport_open));
    extras.insert("mavlink_alive".into(), json!(mavlink_alive));
    extras.insert(
        "heartbeat_age_s".into(),
        fc.heartbeat_age_s()
            .await
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
    );
    extras.insert("fc_source".into(), json!(fc.source()));
    // A human-actionable hint for the not-alive case: msp_detected (the FC speaks
    // MSP, not MAVLink, on this port), no_heartbeat (open but silent), or none.
    let fc_link_hint = fc.link_hint().await;
    extras.insert("fc_link_hint".into(), json!(fc_link_hint));
    // Whether the open FC source is the MAVLink-over-ELRS ingest running
    // telemetry-only, its host->FC command-down direction gated closed. When
    // true the link can read alive (telemetry flows) yet a GCS command is
    // silently dropped — surface it so a consumer never renders a bare
    // "connected" as "commands are getting through". False for every ordinary
    // FC source (serial / UDP / TCP / discovery), whose command path is open.
    extras.insert(
        "fc_command_down_gated".into(),
        json!(fc.command_down_gated()),
    );
    // The FC firmware family identified from the port's USB descriptor
    // (betaflight/inav), or null for a MAVLink/unknown FC — lets a consumer
    // badge "Betaflight (MSP)" instead of a misleading "not connected".
    let fc_variant = fc.fc_variant().await;
    extras.insert(
        "fc_variant".into(),
        fc_variant.as_ref().map(|v| json!(v)).unwrap_or(Value::Null),
    );
    // The honest connected-or-reachable verdict, computed once here (the
    // router owns every input) so every consumer reads the SAME answer
    // instead of re-deriving it from the parts. `/api/status` and
    // `/api/status/full` recompute it from these identical fields today; this
    // is additionally the field the compact aux-lane node-status snapshot
    // reads, so a relayed MSP flight controller (Betaflight/iNav, which never
    // emits a MAVLink heartbeat) is reachable-and-drivable over the radio the
    // same way it already is over a direct LAN connection.
    let fc_reachable = compute_fc_reachable(
        mavlink_alive,
        transport_open,
        fc_variant.is_some(),
        fc_link_hint,
    );
    extras.insert("fc_reachable".into(), json!(fc_reachable));
    // The canonical firmware family (ardupilot/px4/betaflight/inav/unknown):
    // the MSP variant above, or — for a MAVLink FC — the live-heartbeat
    // autopilot code that names ArduPilot vs PX4 (which fc_variant cannot).
    // Read-only classification of already-decoded signals; no payload parsed.
    extras.insert(
        "fc_firmware".into(),
        json!(firmware_family(
            fc_variant.as_deref(),
            mavlink_alive,
            autopilot
        )),
    );
    extras.insert("fc_port".into(), json!(fc.port().await));
    extras.insert("fc_baud".into(), json!(fc.baud()));
    extras.insert(
        "service_uptime".into(),
        json!(started.elapsed().as_secs_f64()),
    );
    extras.insert("param_priming".into(), json!(fc.param_priming()));
    extras.insert(
        "param_sweep_timed_out".into(),
        json!(fc.param_sweep_timed_out()),
    );
    extras.insert(
        "param_sweep_send_failed".into(),
        json!(fc.param_sweep_send_failed()),
    );
    extras.insert("param_cached_count".into(), json!(cached));
    extras.insert("param_expected_count".into(), json!(expected));
    // The parameter map's change counter, NOT the map. The map used to ride here
    // in full — ~24 KB of JSON, no delta, no cap, republished ten times a second
    // — which alone made a relayed `/api/telemetry` need ~21 aux fragments and
    // fail 15% of the time. A consumer compares this counter against its own
    // last-seen value and refetches `GET /api/params` (served from the router's
    // on-disk cache) once on a mismatch. Restart-safe by being wrong in the
    // harmless direction: the counter restarts at 0, which reads as a mismatch
    // and costs exactly one refetch.
    extras.insert("param_generation".into(), json!(param_generation));
    extras.insert("ipc_mavlink_drops".into(), json!(mavlink_drops));
    extras.insert("ipc_state_drops".into(), json!(state_drops));
    // The MAVLink-over-radio tee's full tally: what it forwarded and, just as
    // importantly, everything it dropped and why. Present only where the tee
    // runs, so its absence reads as "not running on this profile" rather than
    // as a lane that carried nothing. Forwarded counts datagrams handed to the
    // kernel, which is not by itself proof the radio radiated them.
    if let Some(counters) = aux_tee_counters {
        extras.insert(
            "aux_mavlink_tee".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // The receiving half's tally: what the republish seam accepted from
    // off-board, what it rejected, and what went nowhere for want of a
    // listener. Present only where the seam runs (a ground station), so its
    // absence reads as "not running on this profile" rather than as a seam that
    // received nothing. Published counts frames handed to the fan-out, which is
    // not by itself proof a ground control station rendered them.
    if let Some(counters) = frame_ingest_counters {
        extras.insert(
            "mavlink_frame_ingest".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // The decoded reading of the relayed vehicle, when this node has ever seen
    // one. Carries its own provenance and freshness (see `crate::relayed`), and
    // is nested rather than merged so it can never be mistaken for the
    // attached-FC fields alongside it. Absent on a node that has never relayed
    // a vehicle, which is a different signal from one whose vehicle has gone
    // quiet (that case is present with `fresh: false`).
    if let Some(relayed) = relayed_vehicle {
        let snapshot = match relayed.lock() {
            Ok(r) => r.to_wire(Instant::now()),
            // Read through a poisoned lock rather than dropping the surface:
            // a blank reading is exactly the failure this projection exists to
            // fix, so it must not be reintroduced by a lock error.
            Err(poisoned) => poisoned.into_inner().to_wire(Instant::now()),
        };
        if let Some(value) = snapshot {
            extras.insert("relayed_vehicle".into(), value);
        }
    }
    // The relay-proxy uplink's tally: how many HTTP requests crossed the radio,
    // how many were dropped as another node's, and — the pair that says whether
    // the ground's retransmission is working — how many arrived as duplicates
    // or were answered from the dedupe cache. Present only on the drone
    // profile, which is the only one that consumes the uplink.
    if let Some(counters) = aux_rpc_counters {
        extras.insert(
            "aux_rpc".into(),
            serde_json::to_value(counters.snapshot()).unwrap_or(Value::Null),
        );
    }
    // The drone's video attention profile, so the swarm beacon's hero bit and the
    // fleet view read one source of truth. Owned by `ados-video`, which stamps
    // the sidecar on every encoder profile apply; this republishes it.
    extras.insert("video_profile".into(), json!(video_profile()));
    // The onboard-autonomy layer's two outputs. `ados-swarmbus` reads both out of
    // this snapshot and folds them into the outgoing beacon, which is how a
    // neighbour and the operator screen learn which layer is ACTUALLY flying this
    // aircraft rather than which one it was commanded into. Absent (the swarm
    // disabled, or no drone slot assigned) reads as the honest pre-Phase-5 state.
    let (precedence, emergency) = match swarm {
        Some(s) => (s.precedence_wire(), s.emergency()),
        None => (ModePrecedence::Hold.as_wire(), false),
    };
    extras.insert(
        ados_swarm_control::EXTRA_PRECEDENCE.into(),
        json!(precedence),
    );
    // A real JSON bool, not 0/1: the beacon builder reads a non-bool as false, and
    // for a safety flag that is the wrong direction to fail in.
    extras.insert(ados_swarm_control::EXTRA_EMERGENCY.into(), json!(emergency));
    // The attitude rung's verdict + counters. Absent (the rung inert / not
    // yet wired) reads as the honest "no-command" hold — never a fabricated
    // default that implies a live rate lane.
    match attitude {
        Some(s) => {
            extras.insert("attitude_verdict".into(), json!(s.verdict_wire()));
            extras.insert(
                "attitude_setpoints_emitted".into(),
                json!(s.setpoints_emitted()),
            );
            extras.insert(
                "attitude_ticks_suppressed".into(),
                json!(s.ticks_suppressed()),
            );
            extras.insert(
                "attitude_freshness_suppressions".into(),
                json!(s.freshness_suppressions()),
            );
        }
        None => {
            extras.insert("attitude_verdict".into(), json!("no-command"));
        }
    }
    extras
}

/// The sidecar `ados-video` stamps on every encoder profile apply. Read here
/// rather than tracked, so a video restart cannot desync the two services.
const VIDEO_PROFILE_SIDECAR: &str = "/run/ados/video-profile.json";

/// The drone's current video attention profile: `"hero"` (full-rate stream) or
/// `"thumbnail"` (1 fps). Anything other than a readable sidecar naming `hero`
/// reads as `thumbnail`, which is the correct boot default — a fleet powering up
/// together must not put 24 drones on a hero's worth of airtime each.
///
/// A tmpfs read at the 10 Hz publish cadence, so the value lags an encoder apply
/// by up to one tick. That is deliberate: the alternative is another IPC hop for
/// a single string.
fn video_profile() -> &'static str {
    let named_hero = std::fs::read_to_string(VIDEO_PROFILE_SIDECAR)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("profile")
                .and_then(Value::as_str)
                .map(|p| p == "hero")
        })
        .unwrap_or(false);
    if named_hero {
        "hero"
    } else {
        "thumbnail"
    }
}

/// Resolve when the service receives SIGTERM or SIGINT.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod fc_reachable_tests {
    use super::compute_fc_reachable;

    #[test]
    fn a_live_mavlink_heartbeat_is_reachable() {
        assert!(compute_fc_reachable(true, true, false, "none"));
        // Even a heartbeat with a stale/odd transport_open reading counts —
        // a HEARTBEAT decoding at all is definitive evidence of a live link.
        assert!(compute_fc_reachable(true, false, false, "none"));
    }

    #[test]
    fn an_identified_msp_fc_on_an_open_port_is_reachable_without_a_heartbeat() {
        // The exact case this fix targets: Betaflight/iNav never emit a
        // MAVLink heartbeat, so mavlink_alive stays false forever on an MSP
        // board, yet it is present and drivable over the byte-transparent
        // proxy.
        assert!(compute_fc_reachable(false, true, true, "none"));
    }

    #[test]
    fn an_open_port_with_only_the_msp_detected_hint_is_reachable() {
        // The hint path: transport open, no decoded variant string yet, but
        // the link monitor's own sniff already says "this looks like MSP".
        assert!(compute_fc_reachable(false, true, false, "msp_detected"));
    }

    #[test]
    fn an_open_port_with_neither_a_heartbeat_nor_msp_evidence_is_not_reachable() {
        // A port that is open but silent, with nothing identifying an FC
        // behind it, must read as absent — this is the "port open, no
        // MAVLink" amber state, not a connected one.
        assert!(!compute_fc_reachable(false, true, false, "none"));
        assert!(!compute_fc_reachable(false, true, false, "no_heartbeat"));
    }

    #[test]
    fn a_closed_transport_is_never_reachable_regardless_of_stale_hints() {
        assert!(!compute_fc_reachable(false, false, true, "msp_detected"));
        assert!(!compute_fc_reachable(false, false, false, "none"));
    }
}

#[cfg(test)]
mod extras_key_set_tests {
    use super::*;

    /// Every key `build_extras` can emit, sorted.
    ///
    /// `ados-control` sorts each of these into exactly one of four dispositions —
    /// `IPC_ONLY_KEYS` (stripped from telemetry), `AGENT_DIAGNOSTIC_KEYS`
    /// (survives the FC-down honesty gate), neither (vehicle-sourced, so gated),
    /// or consumed-and-transformed (removed before the filter and re-emitted in
    /// another shape) — and it cannot depend on this crate to verify its list is
    /// complete. Pinning the set here is that seam: add an extra without
    /// classifying it over there and this test fails.
    ///
    /// `relayed_vehicle` is the only key in the fourth disposition today:
    /// `project_telemetry` removes it and, when it is fresh, projects its nested
    /// vehicle fields up in place of the withheld local ones. So it appears here
    /// but in neither classification list, which is correct rather than an
    /// omission.
    const EXPECTED_EXTRAS_KEYS: [&str; 29] = [
        "attitude_verdict",
        "aux_mavlink_tee",
        "aux_rpc",
        "fc_baud",
        "fc_command_down_gated",
        "fc_connected",
        "fc_firmware",
        "fc_link_hint",
        "fc_port",
        "fc_reachable",
        "fc_source",
        "fc_variant",
        "heartbeat_age_s",
        "ipc_mavlink_drops",
        "ipc_state_drops",
        "mavlink_alive",
        "mavlink_frame_ingest",
        "param_cached_count",
        "param_expected_count",
        "param_generation",
        "param_priming",
        "param_sweep_send_failed",
        "param_sweep_timed_out",
        "relayed_vehicle",
        "service_uptime",
        "swarm_emergency",
        "swarm_precedence",
        "transport_open",
        "video_profile",
    ];

    #[tokio::test]
    async fn build_extras_emits_exactly_the_classified_key_set() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(VehicleState::default()));
        let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
        let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());

        // Every optional counter present at once. No single profile does this
        // (tee + rpc are drone-only, ingest is ground-station-only), but the pin
        // is about the key SET, not about one profile's subset of it.
        //
        // The relayed projection is POPULATED rather than default: an untouched
        // one has never seen a frame, so `to_wire` returns `None` and the key is
        // absent by design. Passing a default here would have pinned a key set
        // that silently omits the relayed lane — which is exactly the shape of
        // the bug this seam exists to catch.
        let relayed = relayed_with_one_frame();
        let extras = build_extras(
            &fc,
            &state,
            &params,
            Instant::now(),
            0,
            0,
            Some(&Arc::new(TeeCounters::default())),
            Some(&Arc::new(IngestCounters::default())),
            Some(&AuxUplinkConsumerCounters::new()),
            Some(&Arc::new(SwarmSetpointStatus::default())),
            None,
            Some(&relayed),
        )
        .await;

        let mut keys: Vec<&str> = extras.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys, EXPECTED_EXTRAS_KEYS,
            "a snapshot key changed; classify it in ados-control's IPC_ONLY_KEYS \
             or AGENT_DIAGNOSTIC_KEYS (or neither, if it is vehicle-sourced, or \
             consumed-and-transformed like relayed_vehicle) before updating this pin"
        );
    }

    /// A relayed projection that has decoded exactly one frame.
    ///
    /// Built through the public `apply_frame` rather than by reaching into the
    /// type, so this helper exercises the same path production does.
    fn relayed_with_one_frame() -> Arc<StdMutex<RelayedVehicle>> {
        use ados_protocol::mavlink::ardupilotmega::{
            MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
        };
        use ados_protocol::mavlink::{serialize_v2, MavHeader, MavMessage};
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        let frame = serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            &msg,
        )
        .unwrap();
        let relayed = RelayedVehicle::default();
        let relayed = Arc::new(StdMutex::new(relayed));
        relayed
            .lock()
            .unwrap()
            .apply_frame(&frame, "2026-08-01T00:00:00+00:00", Instant::now());
        relayed
    }

    /// The producer's half of the cross-crate contract: the key `ados-control`
    /// reads is the key this crate writes.
    ///
    /// Worth its own test because the two live in different crates and the
    /// consumer was merged first — for a while `project_telemetry` read
    /// `relayed_vehicle` off every snapshot while nothing emitted it, so its own
    /// tests passed against hand-built input and production never fired. A
    /// spelling change on either side has to fail somewhere, and this is where.
    #[tokio::test]
    async fn a_populated_relayed_projection_reaches_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(VehicleState::default()));
        let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
        let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());
        let relayed = relayed_with_one_frame();

        let extras = build_extras(
            &fc,
            &state,
            &params,
            Instant::now(),
            0,
            0,
            None,
            Some(&Arc::new(IngestCounters::default())),
            None,
            None,
            None,
            Some(&relayed),
        )
        .await;

        let block = extras
            .get("relayed_vehicle")
            .expect("a populated relayed projection must reach the snapshot");
        assert_eq!(block["source"], "relayed");
        assert_eq!(block["fresh"], true);
        assert_eq!(block["frames_decoded"], 1);
        // Nested, never merged: the relayed reading must not be confusable with
        // the attached-FC fields sitting beside it in the same map.
        assert!(block["vehicle"].is_object());
        assert!(!extras.contains_key("attitude"));
    }

    /// A node that has never relayed a vehicle emits no key at all.
    ///
    /// Absent and present-but-empty are different signals: the first says this
    /// node has never seen a vehicle, the second would say it has one reading
    /// all zeroes. Rendering the second as an aircraft is the failure mode.
    #[tokio::test]
    async fn a_node_that_never_relayed_emits_no_relayed_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(VehicleState::default()));
        let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
        let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());
        let never = Arc::new(StdMutex::new(RelayedVehicle::default()));

        let extras = build_extras(
            &fc,
            &state,
            &params,
            Instant::now(),
            0,
            0,
            None,
            Some(&Arc::new(IngestCounters::default())),
            None,
            None,
            None,
            Some(&never),
        )
        .await;

        assert!(!extras.contains_key("relayed_vehicle"));
    }

    /// Serialized size of an extras map with the WALL-CLOCK keys removed.
    ///
    /// `service_uptime` and `heartbeat_age_s` are floats formatted from
    /// `Instant::now()`, so their digit count differs between two calls made
    /// microseconds apart and the raw byte delta jitters by a couple of bytes.
    /// Measuring the payload-diet property against a clock made this assertion
    /// flaky by construction; the property itself is about the parameter cache, so
    /// the clocks are simply not part of it.
    fn sized_without_clocks(mut extras: Map<String, Value>) -> usize {
        extras.remove("service_uptime");
        extras.remove("heartbeat_age_s");
        serde_json::to_vec(&Value::Object(extras)).unwrap().len()
    }

    /// The payload-diet proof for the `params` blob removal.
    ///
    /// The blob rode the 10 Hz state publish in full, so the snapshot grew
    /// linearly with the FC's parameter count — ~24 KB against ArduPilot's ~700
    /// parameters, which is ~21 aux fragments on a relayed read and the measured
    /// 85% delivery. This measures both shapes against one populated cache: the
    /// current extras, and the same extras with the blob put back. It fails on
    /// the plausible bug (re-adding the map, or letting the counter carry values)
    /// because the current shape's size must not move with the cache size.
    #[tokio::test]
    async fn extras_size_does_not_grow_with_the_cached_parameter_count() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(VehicleState::default()));
        let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
        let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());

        let empty = build_extras(
            &fc,
            &state,
            &params,
            Instant::now(),
            0,
            0,
            Some(&Arc::new(TeeCounters::default())),
            Some(&Arc::new(IngestCounters::default())),
            Some(&AuxUplinkConsumerCounters::new()),
            None,
            None,
            None,
        )
        .await;
        let empty_len = sized_without_clocks(empty);

        // An ArduPilot-scale parameter set: 700 names of realistic length.
        {
            let mut pc = params.lock().await;
            for i in 0..700 {
                pc.set(&format!("ATC_RAT_RLL_P_{i:03}"), 0.135 + i as f64, 9);
            }
        }
        let full = build_extras(
            &fc,
            &state,
            &params,
            Instant::now(),
            0,
            0,
            Some(&Arc::new(TeeCounters::default())),
            Some(&Arc::new(IngestCounters::default())),
            Some(&AuxUplinkConsumerCounters::new()),
            None,
            None,
            None,
        )
        .await;

        // The pre-diet shape: the same extras carrying the whole `{name: value}`
        // map, which is exactly what `params` used to insert.
        let mut pre_diet = full.clone();
        // (`full` is consumed by the measurement below, so the variant is built first.)
        let blob: Map<String, Value> = (0..700)
            .map(|i| (format!("ATC_RAT_RLL_P_{i:03}"), json!(0.135 + i as f64)))
            .collect();
        pre_diet.insert("params".into(), Value::Object(blob));

        let full_len = sized_without_clocks(full);
        let pre_diet_len = sized_without_clocks(pre_diet);
        println!(
            "extras JSON bytes: {empty_len} empty cache, {full_len} with 700 params, \
             {pre_diet_len} with the pre-diet `params` blob"
        );

        // The counter is a scalar, so 700 parameters may only widen the snapshot
        // by the counter's own digits.
        assert!(
            full_len - empty_len <= 4,
            "extras grew {} bytes for 700 cached params; the map is back on the \
             10 Hz publish",
            full_len - empty_len
        );
        // And the shape it replaced was an order of magnitude larger.
        assert!(
            pre_diet_len > 20_000 && pre_diet_len > full_len * 15,
            "pre-diet {pre_diet_len} B vs current {full_len} B"
        );
    }
}
