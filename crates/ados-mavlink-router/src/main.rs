//! `ados-mavlink-router` binary.
//!
//! Owns the FC serial link and serves the MAVLink + state IPC sockets plus the
//! direct-GCS TCP/UDP proxies. Mirrors the Python `ados-mavlink` service
//! (`python -m ados.services.mavlink`): the IPC servers, the FC connection, the
//! 1 Hz companion heartbeat, the 10 Hz state publish, the adaptive stream
//! cadence, and the parameter sweep. The state socket is published as
//! length-prefixed msgpack (v2), the versioned wire the shared reader
//! auto-detects.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::state::encode_v2;
use serde_json::{json, Map, Value};
use tokio::sync::{Mutex, Notify};

use ados_mavlink_router::aux_tee::{self, TeeCounters};
use ados_mavlink_router::aux_uplink;
use ados_mavlink_router::aux_uplink_consumer;
use ados_mavlink_router::config::MavlinkConfig;
use ados_mavlink_router::connection::FcConnection;
use ados_mavlink_router::frame_ingest::{self, IngestCounters, INGEST_QUEUE_DEPTH};
use ados_mavlink_router::param_cache::ParamCache;
use ados_mavlink_router::proxies::{run_tcp_proxy, run_udp_proxy, run_ws_proxy, WsProxyAuth};
use ados_mavlink_router::state::{firmware_family, VehicleState};

const MAVLINK_QUEUE_DEPTH: usize = 256;
const STATE_QUEUE_DEPTH: usize = 32;
const TCP_PROXY_PORT: u16 = 5760;
const UDP_PROXY_PORTS: &[u16] = &[14550, 14551];
/// The ground station's aux-uplink loopback ingress — must equal
/// `ados-groundlink`'s `wfb_rx::args::AUX_TX_PORT`, the port its
/// unconditionally-spawned `wfb_tx -p3` reads from. The two crates do not
/// depend on each other, so this travels as a plain matching literal rather
/// than a shared const (the existing convention for this port pair — see
/// `ados-radio`'s `default_aux_tx_port()` / `ados-groundlink`'s
/// `ATLAS_RX_PORT`, cross-referenced only in comments and tests, never a
/// shared dependency).
const AUX_UPLINK_PORT: u16 = 5602;
/// The drone's own aux-uplink re-emit loopback — must equal `ados-radio`'s
/// `WfbConfig::aux_rx_port` (default 5603), the port its `wfb_rx -p3`
/// re-emits decoded uplink datagrams to. Same cross-crate literal-matching
/// convention as `AUX_UPLINK_PORT` above.
const AUX_UPLINK_REEMIT_PORT: u16 = 5603;

fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
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
        let uplink_counters = aux_uplink_consumer::AuxUplinkConsumerCounters::new();
        let uplink_fc = fc.clone();
        let uplink_cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            aux_uplink_consumer::run(
                AUX_UPLINK_REEMIT_PORT,
                uplink_fc,
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
                // Held for the process lifetime: dropping the server closes the
                // socket and the ground data plane would find nothing to
                // connect to.
                let server = Arc::new(server);
                let fc = fc.clone();
                let cancel = cancel.clone();
                tasks.push(tokio::spawn(async move {
                    let _server = server;
                    frame_ingest::run(inbound, fc, counters, cancel).await
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
        fc.set_aux_uplink(aux_uplink::spawn(AUX_UPLINK_PORT)).await;
    }

    // MAVLink socket client commands -> FC.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = inbound.recv() => match cmd {
                        Some(data) => fc.send_client_bytes(&data).await,
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
                        Some(data) => fc.send_bytes(&data).await,
                        None => break,
                    },
                    _ = cancel.notified() => break,
                }
            }
        }));
    }

    // 10 Hz state publish: vehicle snapshot + the service runtime extras.
    {
        let fc = fc.clone();
        let state = state.clone();
        let params = params.clone();
        let state_ipc = state_ipc.clone();
        let mavlink_ipc_stats = mavlink_ipc.clone();
        let aux_tee_counters = aux_tee_counters.clone();
        let frame_ingest_counters = frame_ingest_counters.clone();
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
        tasks.push(tokio::spawn(async move {
            run_tcp_proxy(fc, port, cancel).await
        }));
    }
    for port in udp_proxy_ports() {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            run_udp_proxy(fc, port, cancel).await
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
) -> Map<String, Value> {
    let cached = params.lock().await.count();
    // The expected param count and the decoded autopilot code, read under one
    // lock. `autopilot` is the already-decoded HEARTBEAT discriminator (3 =
    // ArduPilot, 12 = PX4) used to name the MAVLink firmware family below.
    let (expected, autopilot) = {
        let s = state.lock().await;
        (s.param_count, s.autopilot)
    };
    let params_blob = params.lock().await.get_all();
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
    // Whether a vehicle-command frame (mode/arm/manual-control/setpoint) is
    // refused on the aux-uplink relay rather than radiated toward a linked
    // drone. True by default — a fresh relay carries param/mission/read
    // traffic immediately but stays command-gated until
    // `radio.relay.command_enabled` is explicitly set. Meaningless (always
    // true) off a ground station, since only that profile ever installs an
    // aux-uplink sender; surfaced unconditionally for parity with
    // `fc_command_down_gated` above.
    extras.insert(
        "aux_uplink_command_gated".into(),
        json!(fc.relay_command_gated()),
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
    extras.insert("params".into(), Value::Object(params_blob));
    extras
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
