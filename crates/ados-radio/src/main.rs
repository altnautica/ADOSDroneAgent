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
    hop_announce_rounds, hop_epoch_ms, verify_hop_packet, HopState, HopTrigger, HOP_ACK_PORT,
    HOP_CONTROL_PORT, PRESENCE_INTERVAL,
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
            write_stats_sidecar("disabled", cfg.channel, cfg);
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
            write_stats_sidecar("no_adapter", cfg.channel, cfg);
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
        write_stats_sidecar("connecting", cfg.channel, cfg);
        tracing::info!(iface, channel = cfg.channel, pid, "wfb_service_ready");

        // ── Run watchdogs + hop supervisor concurrently ──────────────────
        let task_cancel = cancel.clone();
        let iface_str = iface.clone();

        let tx_cancel = task_cancel.clone();
        let tx_iface = iface_str.clone();
        let watchdog1 =
            tokio::spawn(async move { tx_health_watchdog(&tx_iface, pid, tx_cancel).await });

        let recvq_cancel = task_cancel.clone();
        let watchdog2 = tokio::spawn(async move { video_recvq_watchdog(recvq_cancel).await });

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
        let beacon = tokio::spawn(async move {
            emit_presence_beacons(&beacon_device, beacon_channel, &beacon_key, beacon_cancel).await
        });

        // Hop supervisor (enabled only when configured).
        let hop_enabled = hop_cfg.auto_hop_enabled;
        let hop = tokio::spawn(async move {
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
        tokio::select! {
            result = watchdog1 => {
                if let Ok(WatchdogFired::TxStalled | WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("watchdog_fired_killing_wfb_tx");
                }
            }
            result = watchdog2 => {
                if let Ok(WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("video_recvq_watchdog_fired");
                }
            }
            _ = hop => {}
            _ = beacon => {}
            _ = cancel.notified() => {
                // Clean shutdown.
                proc.lock().await.kill_all().await;
                tracing::info!("wfb_service_stopping");
                return;
            }
        }

        // Watchdog fired or hop task exited unexpectedly — kill the whole group and respawn.
        proc.lock().await.kill_all().await;
        write_stats_sidecar("connecting", cfg.channel, cfg);
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

/// Minimal FHSS hop supervisor: broadcasts HopAnnounce and listens for HopAck.
/// If ACK arrives, executes: stop wfb_tx → iw set channel → restart wfb_tx.
async fn run_hop_supervisor(
    iface: &str,
    cfg: &WfbConfig,
    proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pair_key: &[u8; 32],
    device_id: &str,
    cancel: Arc<Notify>,
) {
    let mut state = HopState::new(cfg.channel);
    let mut hop_tick = tokio::time::interval(Duration::from_secs(cfg.hop_period_seconds as u64));

    // Listener socket for HopAck (drone receives on 5810).
    let ack_sock = match tokio::net::UdpSocket::bind(format!("0.0.0.0:{HOP_ACK_PORT}")).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, "hop_ack_socket_bind_failed");
            cancel.notified().await;
            return;
        }
    };

    // Broadcast socket for HopAnnounce.
    let announce_sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => {
            cancel.notified().await;
            return;
        }
    };

    let mut ack_buf = [0u8; 64];

    loop {
        tokio::select! {
            _ = hop_tick.tick() => {
                if !state.can_hop() {
                    continue;
                }
                // Scan for the quietest channel (stub: use next channel in band).
                let target = next_candidate_channel(state.channel, cfg);
                if target == state.channel {
                    continue;
                }
                let epoch = hop_epoch_ms();
                let pkt = build_hop_announce(epoch, target, HopTrigger::Periodic, pair_key);

                // Broadcast 30 rounds × 100ms, stop early on ACK.
                let mut acked = false;
                for _ in 0..hop_announce_rounds() {
                    let _ = announce_sock
                        .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
                        .await;
                    // Non-blocking check for ACK.
                    if let Ok(Ok((n, _))) = tokio::time::timeout(
                        hop_announce_interval(),
                        ack_sock.recv_from(&mut ack_buf),
                    )
                    .await
                    {
                        if verify_hop_packet(&ack_buf[..n], pair_key) {
                            acked = true;
                            break;
                        }
                    }
                }
                if acked {
                    // Sleep to epoch, then execute the hop.
                    let epoch_secs = epoch as f64 / 1000.0;
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64();
                    let delay = epoch_secs - now_secs;
                    if delay > 0.0 {
                        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    }
                    proc.lock().await.kill_all().await;
                    set_channel(iface, target).await;
                    match RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY)).await {
                        Ok(new_proc) => {
                            *proc.lock().await = new_proc;
                            state.on_hop(target);
                            tracing::info!(iface, channel = target, "hop_executed");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "hop_wfb_restart_failed");
                        }
                    }
                }
            }
            // Peer stale check — return to home channel.
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                if state.should_return_home() {
                    tracing::info!(home = state.home_channel, "hop_return_home");
                    proc.lock().await.kill_all().await;
                    set_channel(iface, state.home_channel).await;
                    if let Ok(new_proc) =
                        RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY)).await
                    {
                        *proc.lock().await = new_proc;
                        state.on_hop(state.home_channel);
                    }
                }
            }
            // PresenceBeacon inbound (updates the _was_linked gate).
            Ok((n, _)) = ack_sock.recv_from(&mut ack_buf) => {
                let _ = (n, device_id); // consumed to update hop state
                state.on_peer_seen();
            }
            _ = cancel.notified() => return,
        }
    }
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

/// Write the `wfb-stats.json` Contract E sidecar.
fn write_stats_sidecar(state: &str, channel: u8, cfg: &WfbConfig) {
    let v = json!({
        "state": state,
        "channel": channel,
        "tx_power_dbm": cfg.tx_power_dbm,
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        "mcs_index": cfg.mcs_index,
        "profile": "drone",
    });
    let path = run_path("wfb-stats.json");
    let _ = write_sidecar(&path, &v);
}

/// Read the device-id from the standard agent location.
fn read_device_id() -> String {
    std::fs::read_to_string("/etc/ados/device_id")
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
