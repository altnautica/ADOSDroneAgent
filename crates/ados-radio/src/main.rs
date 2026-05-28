//! `ados-radio` binary — the WFB TX service for the drone profile.
//!
//! Mirrors `python -m ados.services.wfb` (drone profile path):
//! waits for the WFB TX key, selects the injection adapter, sets monitor mode,
//! spawns `wfb_tx` in its own process group (setsid + killpg — the structural
//! fix for the orphaned-wfb_tx bug class), runs the Rule-37 watchdogs and the
//! FHSS hop supervisor UDP tasks, writes Contract E sidecars, and shuts down
//! cleanly on SIGTERM/SIGINT.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;

#[cfg(target_os = "linux")]
use ados_radio::adapter;
use ados_radio::config::WfbConfig;
use ados_radio::hop::{
    build_hop_announce, build_presence_beacon, derive_pair_key, hop_announce_interval,
    hop_announce_rounds, hop_epoch_ms, parse_hop_ack, parse_presence_beacon, HopState, HopTrigger,
    HOP_ACK_PORT, HOP_CONTROL_PORT, PRESENCE_INTERVAL,
};
use ados_radio::paths::{run_path, write_sidecar, DRONE_KEY, WFB_TX_KEY};
use ados_radio::process::RadioProcesses;
use ados_radio::watchdog::{tx_health_watchdog, video_recvq_watchdog, WatchdogFired};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Poll interval while waiting for the WFB TX key (unpaired state).
const KEY_WAIT_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!("wfb_service_starting");

    let cfg = WfbConfig::load_from(Path::new(CONFIG_YAML));
    let cancel = Arc::new(Notify::new());

    // ── Signal handler ────────────────────────────────────────────────────
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            wait_for_shutdown().await;
            cancel.notify_waiters();
        });
    }

    run_service(&cfg, cancel).await;
    tracing::info!("wfb_service_stopped");
}

async fn run_service(cfg: &WfbConfig, cancel: Arc<Notify>) {
    loop {
        // ── Key guard — block while unpaired ─────────────────────────────
        if !Path::new(WFB_TX_KEY).exists() {
            tracing::info!(key = WFB_TX_KEY, "wfb_blocked_unpaired");
            write_stats_sidecar("disabled", cfg.channel, cfg.tx_power_dbm, None, cfg);
            tokio::select! {
                _ = tokio::time::sleep(KEY_WAIT_INTERVAL) => continue,
                _ = cancel.notified() => return,
            }
        }

        // ── Adapter selection ─────────────────────────────────────────────
        let selected: Option<ados_radio::adapter::SelectedAdapter> = {
            #[cfg(target_os = "linux")]
            {
                adapter::select_interface(&cfg.interface).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
        };
        let Some(adapter) = selected else {
            tracing::warn!("wfb_no_adapter_found");
            write_stats_sidecar("no_adapter", cfg.channel, cfg.tx_power_dbm, None, cfg);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                _ = cancel.notified() => return,
            }
        };

        tracing::info!(
            iface = %adapter.ifname,
            chipset = %adapter.chipset,
            injection_ok = adapter.injection_ok,
            "adapter_selected"
        );

        // ── Set channel via iw ────────────────────────────────────────────
        let iface = &adapter.ifname;
        set_channel(iface, cfg.channel).await;

        // ── Clamp TX power BEFORE wfb_tx starts injecting ─────────────────
        // Critical on host-VBUS rigs: the driver default (~17-20 dBm) browns
        // out the adapter. Ramps up from the configured floor on rejection.
        let effective_tx_dbm = ados_radio::adapter::set_tx_power(iface, cfg.tx_power_dbm)
            .await
            .unwrap_or(cfg.tx_power_dbm);

        // ── Load pair key for HMAC derivation ────────────────────────────
        let drone_key = tokio::fs::read(DRONE_KEY).await.ok();
        let pair_key = derive_pair_key(drone_key.as_deref());

        // ── Spawn the radio process group: data wfb_tx + tx/rx control ────
        // (each in its own session — the orphan fix; control plane carries
        // HopAnnounce/HopAck over the air, so it MUST run for FHSS to work.)
        let key_path = Path::new(WFB_TX_KEY);
        let proc = match RadioProcesses::spawn(iface, cfg, key_path).await {
            Ok(p) => Arc::new(tokio::sync::Mutex::new(p)),
            Err(e) => {
                tracing::warn!(error = %e, "wfb_spawn_failed");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                    _ = cancel.notified() => return,
                }
            }
        };
        let pid = { proc.lock().await.data_tx_pid().unwrap_or(0) };
        let adapter_info = AdapterInfo {
            interface: iface.clone(),
            chipset: adapter.chipset.clone(),
            injection_ok: adapter.injection_ok,
        };
        write_stats_sidecar(
            "connecting",
            cfg.channel,
            effective_tx_dbm,
            Some(&adapter_info),
            cfg,
        );
        tracing::info!(iface, channel = cfg.channel, pid, "wfb_service_ready");

        // ── Run watchdogs + hop supervisor concurrently ──────────────────
        let task_cancel = cancel.clone();
        let iface_str = iface.clone();

        // 2 s sidecar heartbeat — keeps wfb-stats.json fresh so the REST
        // handler never marks it stale (mtime > 10 s).
        let hb_cancel = task_cancel.clone();
        let hb_cfg = cfg.clone();
        let hb_adapter = adapter_info.clone();
        let hb_channel = cfg.channel;
        let mut heartbeat = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        write_stats_sidecar(
                            "connecting",
                            hb_channel,
                            effective_tx_dbm,
                            Some(&hb_adapter),
                            &hb_cfg,
                        );
                    }
                    _ = hb_cancel.notified() => break,
                }
            }
        });

        let tx_cancel = task_cancel.clone();
        let tx_iface = iface_str.clone();
        let mut watchdog1 =
            tokio::spawn(async move { tx_health_watchdog(&tx_iface, pid, tx_cancel).await });

        let recvq_cancel = task_cancel.clone();
        let mut watchdog2 = tokio::spawn(async move { video_recvq_watchdog(recvq_cancel).await });

        let hop_cancel = task_cancel.clone();
        let hop_iface = iface_str.clone();
        let hop_proc = proc.clone();
        let hop_cfg = cfg.clone();
        let hop_key = pair_key;
        let presence_cancel = task_cancel.clone();
        let device_id = read_device_id();

        // Presence beacon emitter (10s interval).
        let beacon_cancel = presence_cancel.clone();
        let beacon_key = hop_key;
        let beacon_channel = cfg.channel;
        let beacon_device = device_id.clone();
        let mut beacon = tokio::spawn(async move {
            emit_presence_beacons(&beacon_device, beacon_channel, &beacon_key, beacon_cancel).await
        });

        // Hop supervisor (enabled only when configured).
        let hop_enabled = hop_cfg.auto_hop_enabled;
        let mut hop = tokio::spawn(async move {
            if hop_enabled {
                run_hop_supervisor(
                    &hop_iface, &hop_cfg, hop_proc, &hop_key, &device_id, hop_cancel,
                )
                .await;
            } else {
                hop_cancel.notified().await;
            }
        });

        // Wait for any task to finish (cancel → shutdown; watchdog → respawn).
        // `&mut` the handles so the un-selected ones are NOT dropped-and-detached
        // here — we abort them explicitly below so tasks don't pile up across
        // respawns.
        tokio::select! {
            result = &mut watchdog1 => {
                if let Ok(WatchdogFired::TxStalled | WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("watchdog_fired_killing_wfb_tx");
                }
            }
            result = &mut watchdog2 => {
                if let Ok(WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("video_recvq_watchdog_fired");
                }
            }
            _ = &mut hop => {}
            _ = &mut beacon => {}
            _ = &mut heartbeat => {}
            _ = cancel.notified() => {
                // Clean shutdown: stop the tasks then the radio group.
                heartbeat.abort();
                watchdog1.abort();
                watchdog2.abort();
                hop.abort();
                beacon.abort();
                proc.lock().await.kill_all().await;
                tracing::info!("wfb_service_stopping");
                return;
            }
        }

        // A task exited (watchdog fired / hop ended) — abort the siblings so they
        // don't accumulate, kill the whole radio group, and respawn.
        heartbeat.abort();
        watchdog1.abort();
        watchdog2.abort();
        hop.abort();
        beacon.abort();
        proc.lock().await.kill_all().await;
        write_stats_sidecar(
            "connecting",
            cfg.channel,
            effective_tx_dbm,
            Some(&adapter_info),
            cfg,
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = cancel.notified() => return,
        }
    }
}

/// Emit PresenceBeacons on UDP 127.0.0.1:5803 every 10s.
async fn emit_presence_beacons(
    device_id: &str,
    channel: u8,
    pair_key: &[u8; 32],
    cancel: Arc<Notify>,
) {
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
        return;
    };
    let mut tick = tokio::time::interval(PRESENCE_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let epoch = hop_epoch_ms();
                let pkt = build_presence_beacon(
                    device_id,
                    true, // drone role
                    channel,
                    0,    // rssi not known at drone-side TX
                    epoch,
                    pair_key,
                );
                let _ = sock
                    .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
                    .await;
            }
            _ = cancel.notified() => return,
        }
    }
}

/// FHSS hop supervisor. A dedicated 5810 listener decodes the control plane
/// (HopAck + the peer's PresenceBeacon) and drives the shared `HopState`; the
/// hop loop announces a target, waits for the matching ACK, then executes the
/// channel change. Writes `hop-supervisor.json` (5 s) + `peer-presence.json`.
async fn run_hop_supervisor(
    iface: &str,
    cfg: &WfbConfig,
    proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pair_key: &[u8; 32],
    _device_id: &str,
    cancel: Arc<Notify>,
) {
    let state = Arc::new(tokio::sync::Mutex::new(HopState::new(cfg.channel)));
    let pair_key = *pair_key; // [u8;32] is Copy — move into tasks freely.

    // ── Control-plane listener on 5810: HopAck vs PresenceBeacon ──────────
    let ack_sock = match tokio::net::UdpSocket::bind(format!("0.0.0.0:{HOP_ACK_PORT}")).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, "hop_ack_socket_bind_failed");
            cancel.notified().await;
            return;
        }
    };
    // Acked target channels flow from the listener to the hop loop.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<u8>(8);
    let lst_state = state.clone();
    let lst_cancel = cancel.clone();
    let lst_sock = ack_sock.clone();
    let listener = tokio::spawn(async move {
        let mut buf = [0u8; 128];
        loop {
            tokio::select! {
                r = lst_sock.recv_from(&mut buf) => {
                    let Ok((n, _)) = r else { continue };
                    let pkt = &buf[..n];
                    if let Some(target) = parse_hop_ack(pkt, &pair_key) {
                        let _ = ack_tx.try_send(target);
                    } else if let Some(p) = parse_presence_beacon(pkt, &pair_key) {
                        lst_state.lock().await.on_peer_beacon(p);
                        write_peer_presence_json(&lst_state).await;
                    }
                }
                _ = lst_cancel.notified() => break,
            }
        }
    });

    // ── hop-supervisor.json writer (5 s) ──────────────────────────────────
    let hb_state = state.clone();
    let hb_cancel = cancel.clone();
    let hb_cfg = cfg.clone();
    let hb_writer = tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = t.tick() => write_hop_supervisor_json(&hb_state, &hb_cfg).await,
                _ = hb_cancel.notified() => break,
            }
        }
    });

    let announce_sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => {
            listener.abort();
            hb_writer.abort();
            cancel.notified().await;
            return;
        }
    };

    let mut hop_tick = tokio::time::interval(Duration::from_secs(cfg.hop_period_seconds as u64));
    let mut stale_tick = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = hop_tick.tick() => {
                if !state.lock().await.can_hop() {
                    continue;
                }
                let cur = state.lock().await.channel;
                let target = next_candidate_channel(cur, cfg);
                if target == cur {
                    continue;
                }
                let epoch = hop_epoch_ms();
                let pkt = build_hop_announce(epoch, target, HopTrigger::Periodic, &pair_key);
                // Drain any stale acks so we only count one for THIS announce.
                while ack_rx.try_recv().is_ok() {}

                // Announce up to 30×@100ms, stop early on the matching ACK.
                let mut acked = false;
                for _ in 0..hop_announce_rounds() {
                    let _ = announce_sock
                        .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
                        .await;
                    if let Ok(Some(ch)) =
                        tokio::time::timeout(hop_announce_interval(), ack_rx.recv()).await
                    {
                        if ch == target {
                            acked = true;
                            break;
                        }
                    }
                }
                if acked {
                    sleep_to_epoch(epoch).await;
                    proc.lock().await.kill_all().await;
                    set_channel(iface, target).await;
                    match RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY)).await {
                        Ok(new_proc) => {
                            *proc.lock().await = new_proc;
                            state.lock().await.record_hop(target, "periodic", true);
                            tracing::info!(iface, channel = target, "hop_executed");
                        }
                        Err(e) => {
                            state.lock().await.record_hop(target, "periodic", false);
                            tracing::warn!(error = %e, "hop_wfb_restart_failed");
                        }
                    }
                }
            }
            // Peer-stale check — return to home channel (no ACK handshake).
            _ = stale_tick.tick() => {
                let (return_home, home) = {
                    let s = state.lock().await;
                    (s.should_return_home(), s.home_channel)
                };
                if return_home {
                    tracing::info!(home, "hop_return_home");
                    proc.lock().await.kill_all().await;
                    set_channel(iface, home).await;
                    let ok = match RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY)).await {
                        Ok(new_proc) => {
                            *proc.lock().await = new_proc;
                            true
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "return_home_restart_failed");
                            false
                        }
                    };
                    state.lock().await.record_hop(home, "return_home", ok);
                }
            }
            _ = cancel.notified() => {
                listener.abort();
                hb_writer.abort();
                return;
            }
        }
    }
}

/// Sleep until the hop epoch (wall-clock ms). No-op if the epoch is past.
async fn sleep_to_epoch(epoch_ms: u64) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let delay = (epoch_ms as f64 / 1000.0) - now_secs;
    if delay > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }
}

/// Write `peer-presence.json` (Contract E) from the shared hop state.
async fn write_peer_presence_json(state: &Arc<tokio::sync::Mutex<HopState>>) {
    let v = {
        let s = state.lock().await;
        match s.peer() {
            Some(p) => json!({
                "peer_device_id": p.device_id,
                "peer_role": p.role,
                "peer_channel": p.channel,
                "peer_rssi_dbm": p.rssi_dbm,
                "peer_last_seen_unix": s.peer_last_seen_unix(),
            }),
            None => json!({
                "peer_device_id": serde_json::Value::Null,
                "peer_role": serde_json::Value::Null,
                "peer_channel": serde_json::Value::Null,
                "peer_rssi_dbm": serde_json::Value::Null,
                "peer_last_seen_unix": serde_json::Value::Null,
            }),
        }
    };
    let _ = write_sidecar(&run_path("peer-presence.json"), &v);
}

/// Write `hop-supervisor.json` (Contract E) from the shared hop state + config.
async fn write_hop_supervisor_json(state: &Arc<tokio::sync::Mutex<HopState>>, cfg: &WfbConfig) {
    let v = {
        let s = state.lock().await;
        let history =
            serde_json::to_value(s.history()).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
        json!({
            "enabled": cfg.auto_hop_enabled,
            "band": cfg.band,
            "hop_period_seconds": cfg.hop_period_seconds,
            "loss_threshold_percent": cfg.hop_loss_threshold_percent as f64,
            "rssi_threshold_dbm": cfg.hop_rssi_threshold_dbm as f64,
            "last_hop_at": s.last_hop_at_unix(),
            "history": history,
            "wall_time_unix": ados_radio::hop::now_unix(),
        })
    };
    let _ = write_sidecar(&run_path("hop-supervisor.json"), &v);
}

/// Pick the next candidate channel in the configured band (simple rotation;
/// the full Python version uses iw scan for the quietest channel).
fn next_candidate_channel(current: u8, cfg: &WfbConfig) -> u8 {
    let unii3 = [149u8, 153, 157, 161, 165];
    let unii1 = [36u8, 40, 44, 48];
    let candidates: &[u8] = if cfg.band.contains("unii-1") || cfg.band.contains("u-nii-1") {
        &unii1
    } else {
        &unii3
    };
    candidates
        .iter()
        .find(|&&c| c != current)
        .copied()
        .unwrap_or(current)
}

/// `iw <iface> set channel <ch>` (best-effort; failures are logged).
async fn set_channel(iface: &str, channel: u8) {
    let result = tokio::process::Command::new("iw")
        .args([iface, "set", "channel", &channel.to_string()])
        .status()
        .await;
    match result {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(iface, channel, exit = s.code(), "iw_set_channel_failed"),
        Err(e) => tracing::warn!(iface, channel, error = %e, "iw_set_channel_error"),
    }
}

/// The adapter facts the sidecar surfaces (None until an adapter is selected).
#[derive(Clone, Default)]
struct AdapterInfo {
    interface: String,
    chipset: String,
    injection_ok: bool,
}

/// Write the `wfb-stats.json` Contract E sidecar (full schema the REST handler
/// at `api/routes/wfb.py` merges over its base, so the GCS/LCD/dashboard radio
/// panel renders correctly). The link-quality fields (rssi/snr/packets/loss/
/// bitrate) are left to the REST base defaults until the link-quality monitor
/// lands; `adapter_chipset`/`adapter_injection_ok`/`tx_power_dbm` must be
/// present here or the panel shows a false "stranded radio" warning. Re-written
/// on a 2 s cadence so the handler's `mtime > 10 s → state="stale"` never trips.
fn write_stats_sidecar(
    state: &str,
    channel: u8,
    effective_tx_dbm: i8,
    adapter: Option<&AdapterInfo>,
    cfg: &WfbConfig,
) {
    let (interface, chipset, injection_ok) = match adapter {
        Some(a) => (a.interface.as_str(), a.chipset.as_str(), a.injection_ok),
        None => ("", "", false),
    };
    let v = json!({
        "state": state,
        "interface": interface,
        "channel": channel,
        "adapter_chipset": chipset,
        "adapter_injection_ok": injection_ok,
        "tx_power_dbm": effective_tx_dbm,
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        "mcs_index": cfg.mcs_index,
        "channel_locked": true,
        "profile": "drone",
    });
    let path = run_path("wfb-stats.json");
    let _ = write_sidecar(&path, &v);
}

/// Read the device-id from the canonical agent location (`/etc/ados/device-id`,
/// hyphen — matches `core/paths.py:122 DEVICE_ID_PATH`).
fn read_device_id() -> String {
    std::fs::read_to_string("/etc/ados/device-id")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Resolve when SIGTERM or SIGINT is received.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
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
