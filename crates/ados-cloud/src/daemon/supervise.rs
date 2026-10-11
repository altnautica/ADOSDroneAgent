//! Heartbeat and relay supervisors spawned by the cloud relay daemon.

use super::*;

/// The HAL board sidecar the API process persists once per boot. Read here so the
/// native base carries the real board identity (name/tier/soc/arch) instead of
/// "unknown", even before the enrichment loop's first write.
pub(super) const BOARD_SIDECAR: &str = "/run/ados/board.json";

/// The schema version this build reads for the board sidecar. Must match the
/// `board` entry in the shared contract registry (asserted by a test).
pub(super) const BOARD_SIDECAR_VERSION: u16 = 1;

/// Board identity for the heartbeat base, read from [`BOARD_SIDECAR`]. Falls back
/// to "unknown"/0/"" when the file is absent or malformed — the same degraded
/// shape the loop emitted before, but truthful whenever the board has been
/// detected (the normal case once the API service has served one status).
pub(super) fn board_base() -> (String, i64, String, String, f64, bool) {
    let parsed: Option<serde_json::Value> = std::fs::read_to_string(BOARD_SIDECAR)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    // Best-effort schema-drift signal: an older/foreign board file with no
    // `version` key reads back as 0 and warns, but is still used. Never a reject.
    if let Some(v) = parsed.as_ref() {
        let got = v
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u16;
        ados_protocol::sidecar::check_sidecar_version("board", got, BOARD_SIDECAR_VERSION);
    }
    let obj = parsed.as_ref().and_then(|v| v.as_object());
    let s = |k: &str| {
        obj.and_then(|o| o.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let tier = obj
        .and_then(|o| o.get("tier"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let npu_tops = obj
        .and_then(|o| o.get("npu_tops"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    // The board profile's CPU-ONNX local-inference declaration; absent on an
    // older sidecar ⇒ false (the perception tier is unchanged there).
    let local_inference = obj
        .and_then(|o| o.get("has_local_inference"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    (
        s("name").unwrap_or_else(|| "unknown".to_string()),
        tier,
        s("soc").unwrap_or_default(),
        s("arch").unwrap_or_default(),
        npu_tops,
        local_inference,
    )
}

/// Spawn the heartbeat loop: when paired, POST the enriched payload every 5 s.
///
/// This is the node's ONLY `/agent/status` producer. On a ground station it
/// folds the relay state the GS bridge publishes on `relay_state` (uplink,
/// broker session, throttle, forwarding flags, vehicle telemetry) onto the same
/// document, so the relay block and the board/radio/service enrichment reach
/// the row together instead of two producers overwriting each other's absences.
pub(super) fn spawn_heartbeat(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    relay_state: watch::Receiver<Option<gs_bridge::GsHeartbeat>>,
    drone_link: watch::Receiver<Option<Arc<AtomicBool>>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(heartbeat::HEARTBEAT_INTERVAL);
        // The previous tick's /proc/stat sample, kept across ticks so the native
        // enrichment producer reports a true inter-tick CPU delta (omitted on the
        // first tick, which has no prior sample to delta against).
        let mut prev_cpu: Option<enrichment::CpuSample> = None;
        let mut link = heartbeat::CloudLinkTracker::default();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let api_key = pairing.api_key();
                    if !should_emit(api_key, &convex_url) {
                        write_cloud_link(&link, api_key.is_some(), &convex_url, None);
                        continue;
                    }
                    let api_key = api_key.expect("should_emit gates on api_key being Some");
                    let (
                        board_name,
                        board_tier,
                        board_soc,
                        board_arch,
                        board_npu_tops,
                        board_local_inference,
                    ) = board_base();
                    // The profile in WIRE form. Posting the internal form told
                    // the cloud `ground_station` or `auto`, neither of which the
                    // fleet view classifies.
                    let base = heartbeat::HeartbeatBase {
                        device_id: config.agent.device_id.clone(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        profile: Some(config.wire_profile()),
                        role: None,
                        uptime_seconds: started.elapsed().as_secs() as i64,
                        board_name,
                        board_tier,
                        board_soc,
                        board_arch,
                        board_npu_tops,
                        board_local_inference,
                    };
                    // Live status (resources + FC link + service fleet) built in
                    // Rust from the real sources each tick, folded over the base.
                    // A blocking call (reads /proc, the state socket, shells
                    // systemctl) — keep it off the async reactor on the blocking
                    // pool, carrying the CPU sample in and back out.
                    let (enrich, next_cpu) = tokio::task::spawn_blocking(move || {
                        let mut cpu = prev_cpu;
                        let enrich = enrichment::build_native_enrichment(&mut cpu);
                        (enrich, cpu)
                    })
                    .await
                    .unwrap_or((serde_json::Value::Null, prev_cpu));
                    prev_cpu = next_cpu;
                    let mut body = heartbeat::build_payload(&base, Some(&enrich));
                    // Fold the ground-station relay block, when the bridge has
                    // published one. Absent on a drone, so the drone payload is
                    // byte-unchanged. The bridge owns `profile`/`role` for a GS
                    // (it reads the live mesh role, which this loop does not),
                    // and `deviceId`/`version`/`uptimeSeconds` were already
                    // re-asserted from the base by `build_payload`, so the fold
                    // cannot divert the row's identity.
                    fold_relay_state(&mut body, relay_state.borrow().as_ref());
                    let outcome = heartbeat::post_heartbeat(&http, &convex_url, api_key, &body).await;
                    link.record(&outcome, now_epoch_ms());
                    // A GS reports its bridge's session; a drone its relay's.
                    let broker = relay_state
                        .borrow()
                        .as_ref()
                        .map(|r| r.mqtt_connected)
                        .or_else(|| {
                            drone_link
                                .borrow()
                                .as_ref()
                                .map(|f| f.load(std::sync::atomic::Ordering::Acquire))
                        });
                    write_cloud_link(&link, true, &convex_url, broker);
                }
            }
        }
    })
}

/// Rewrite the cloud-link sidecar for this tick. Best-effort: a tmpfs write
/// failure is logged and the next tick tries again.
pub(super) fn write_cloud_link(
    link: &heartbeat::CloudLinkTracker,
    paired: bool,
    convex_url: &str,
    broker_connected: Option<bool>,
) {
    let record = link.link(
        paired,
        !convex_url.is_empty(),
        broker_connected,
        now_epoch_ms(),
    );
    if let Err(e) = ados_protocol::cloud_link::write_cloud_link(&record) {
        tracing::debug!(error = %e, "cloud_link_sidecar_write_failed");
    }
}

/// Fold the ground-station relay block onto the heartbeat body in place.
///
/// `None` (a drone, or a GS bridge that has not published yet) leaves the body
/// untouched, so the drone payload is byte-unchanged. The relay slice's keys are
/// already the camelCase wire names the status mutation declares, so they are
/// copied over verbatim — except the three identity keys, which the heartbeat
/// base owns and which a folded producer must never be able to divert.
pub(super) fn fold_relay_state(
    body: &mut serde_json::Value,
    relay: Option<&gs_bridge::GsHeartbeat>,
) {
    const BASE_OWNED: [&str; 3] = ["deviceId", "version", "uptimeSeconds"];
    let Some(relay) = relay else { return };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Ok(serde_json::Value::Object(slice)) = serde_json::to_value(relay) else {
        return;
    };
    for (k, v) in slice {
        if BASE_OWNED.contains(&k.as_str()) {
            continue;
        }
        obj.insert(k, v);
    }
}

/// Epoch milliseconds from the system clock.
pub(super) fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawn the drone-side relay supervisor: while paired and a broker is
/// configured for this posture, keep the MAVLink relay, the MSP byte plane and
/// the WebRTC signaling lane connected over MQTT; on exit, restart them after a
/// short fixed delay. Each run stops when the pairing key changes or is
/// removed, so an unpair stops the relay and a re-pair dials with the new key.
pub(super) fn spawn_drone_relay(
    config: Arc<CloudConfig>,
    link: watch::Sender<Option<Arc<AtomicBool>>>,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let link = Arc::new(link);
    tokio::spawn(relay_lifecycle::supervise(
        shutdown,
        current_pairing_key,
        relay_lifecycle::PAIRING_POLL_INTERVAL,
        relay_lifecycle::RELAY_SETTLE,
        move |key, run_shutdown| {
            // A posture with no broker: nothing to relay.
            let transport = config.relay_transport(None, &key)?;
            let device_id = config.agent.device_id.clone();
            let link = link.clone();
            Some(async move {
                // The MSP byte plane and the WebRTC signaling lane run alongside
                // the MAVLink frame plane on the same broker, each on its own
                // ClientID. They share the run's shutdown watch and are stopped
                // when the MAVLink relay returns so each run spawns a clean set.
                let msp_relay = MspMqttRelay::new(device_id.clone(), transport.clone());
                let msp_shutdown = run_shutdown.clone();
                let msp_task = tokio::spawn(async move {
                    if let Err(e) = msp_relay.run(gs_bridge::MSP_SOCK, msp_shutdown).await {
                        tracing::warn!(error = %e, "drone msp relay exited");
                    }
                });
                let signaling_cfg = transport.clone();
                let signaling_id = device_id.clone();
                let signaling_shutdown = run_shutdown.clone();
                let signaling_task = tokio::spawn(async move {
                    // A drone has no data cap: every offer is served.
                    let video_allowed = Arc::new(AtomicBool::new(true));
                    if let Err(e) = run_webrtc_signaling(
                        &signaling_id,
                        &signaling_cfg,
                        video_allowed,
                        signaling_shutdown,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "drone webrtc signaling exited");
                    }
                });
                let relay = MavlinkMqttRelay::new(device_id, transport);
                tracing::info!("drone mavlink relay connecting");
                if let Err(e) = relay
                    .run_observed(gs_bridge::MAVLINK_SOCK, run_shutdown, Some(&*link))
                    .await
                {
                    tracing::warn!(error = %e, "drone mavlink relay exited");
                }
                // The MAVLink relay returned (shutdown, pairing change or exit):
                // stop the sibling lanes too.
                msp_task.abort();
                signaling_task.abort();
                // The session ended with the relay; report no session.
                let _ = link.send(None);
            })
        },
    ))
}

/// The pairing key the relays authenticate with, re-read from disk.
pub(super) fn current_pairing_key() -> Option<String> {
    PairingState::load().api_key().map(str::to_string)
}

/// Spawn the ground-station cloud relay bridge: uplink-aware MQTT supervision
/// (explicit teardown/reconnect on every uplink change), data-cap downshift, and
/// the 30 s relay-state republish. Runs only while paired; each bridge run ends
/// when the pairing key changes or is removed, then the pair state is re-checked.
///
/// The bridge does NOT POST `/agent/status`: it publishes its relay state on
/// `relay_state_tx` and the heartbeat loop — the node's single producer — folds
/// it. Two producers on one row is not a merge; each tick's absences overwrote
/// the other's readings.
pub(super) fn spawn_gs_bridge(
    config: Arc<CloudConfig>,
    relay_state_tx: watch::Sender<Option<gs_bridge::GsHeartbeat>>,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The live vehicle-state reader enriches the GS relay telemetry.
        let state_reader = Arc::new(gs_bridge::StateIpcReader::spawn(
            std::path::PathBuf::from(gs_bridge::STATE_SOCK),
            shutdown.clone(),
        ));
        relay_lifecycle::supervise(
            shutdown,
            current_pairing_key,
            relay_lifecycle::PAIRING_POLL_INTERVAL,
            relay_lifecycle::RELAY_SETTLE,
            move |key, run_shutdown| {
                let transport = config.relay_transport(None, &key)?;
                let mut bridge = CloudRelayBridge::new(
                    config.agent.device_id.clone(),
                    PairingState::load().owner_id,
                    transport,
                )
                .with_state_source(state_reader.clone())
                .with_relay_state_sink(relay_state_tx.clone());
                Some(async move { bridge.run(run_shutdown).await })
            },
        )
        .await
    })
}
