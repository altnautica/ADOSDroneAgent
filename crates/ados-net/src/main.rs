//! `ados-net` daemon — the ground-station uplink matrix.
//!
//! Wires the full uplink stack:
//!   - the priority-failover router FSM over the real ethernet + Wi-Fi-client
//!     managers (the HW-gated cellular slot stays a stub until the modem chunk),
//!   - the active-uplink sidecar (written inside the router's tick/switch),
//!   - the cellular data-cap tracker polling at 60 s, publishing threshold
//!     events on the router's bus,
//!   - the share-uplink firewall throttle consumer that turns those threshold
//!     events into tc / NAT actions on the active iface,
//!   - the hostapd AP manager (LAN side) and the USB-gadget tether manager,
//!     each brought up at start and torn down on shutdown.
//!
//! Modeled on the `ados-cloud` binary shape: journald logging on Linux with an
//! fmt fallback. The modem manager (zbus/AT, HW-gated) lands in the last chunk.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use ados_net::cmd::TokioCmdRunner;
use ados_net::data_cap::{DataCapTracker, SysfsUsageSource, DATA_CAP_INTERVAL};
use ados_net::managers::{
    desired_modem_session, EthernetManager, HostapdManager, ModemConfig, ModemManager,
    ModemSession, UsbGadgetManager, WifiClientManager,
};
use ados_net::router::failover;
use ados_net::sysfs::detect_ethernet_iface;
use ados_net::{
    run_share_uplink_consumer, run_throttle_consumer, ShareUplinkFirewall, UplinkManager,
    UplinkRouter,
};

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-net"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-net"))
        .try_init();
}

/// Notify systemd of readiness (no-op off Linux / outside a notify unit).
#[cfg(target_os = "linux")]
fn notify_ready() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
}

#[cfg(not(target_os = "linux"))]
fn notify_ready() {}

/// Read the device id from `/etc/ados/device-id` (trimmed). Empty on a board
/// that has not been provisioned yet, which yields the all-zeros SSID suffix.
fn read_device_id() -> String {
    std::fs::read_to_string(ados_net::paths::DEVICE_ID_PATH)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Wire the real ethernet + Wi-Fi-client + cellular modem managers. The modem
/// fills the `wwan0` slot; its `is_up` only reports kernel-iface liveness, so
/// the router never auto-connects it (bring-up is an explicit, config-gated
/// step in `main`). `usb0` has no manager (the FSM checks its sysfs carrier
/// directly). The `ModemManager` is returned separately too so `main` can gate
/// its bring-up on the sidecar.
fn build_managers(
    runner: Arc<TokioCmdRunner>,
    modem: Arc<ModemManager>,
) -> HashMap<String, Arc<dyn UplinkManager>> {
    let eth_iface = detect_ethernet_iface();
    let mut m: HashMap<String, Arc<dyn UplinkManager>> = HashMap::new();
    m.insert(
        "eth0".to_string(),
        Arc::new(EthernetManager::new(eth_iface, runner.clone())),
    );
    m.insert(
        "wlan0_client".to_string(),
        Arc::new(WifiClientManager::new(runner.clone())),
    );
    m.insert("wwan0".to_string(), modem);
    m
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let runner = Arc::new(TokioCmdRunner);
    let device_id = read_device_id();
    let priority = failover::load_priority(ados_net::paths::gs_uplink_json());
    tracing::info!(
        priority = ?priority,
        host = ados_net::health::HEALTH_HOST,
        "uplink router starting"
    );

    // Durable-store emitter: ships the active-uplink + data-cap snapshots to the
    // logging daemon's ingest socket alongside their sidecars, so a store-first
    // reader sees the daemon's truth even when the in-FastAPI-process view is
    // degraded. Best-effort; spawned on this runtime (must be in a runtime
    // context, which #[tokio::main] guarantees). Cloned freely — every clone
    // shares the one shipper channel.
    let emitter = ados_protocol::logd::emitter::IngestEmitter::new("ados-net");

    let modem = Arc::new(ModemManager::new());
    let router = Arc::new(UplinkRouter::new_with_emitter(
        build_managers(runner.clone(), Arc::clone(&modem)),
        Some(priority),
        None,
        emitter.clone(),
    ));

    // Share-uplink firewall + the data-cap throttle bridge. Subscribe to the
    // router's bus BEFORE spawning the consumers so an event published right
    // after the spawn is not lost to the broadcast channel.
    let firewall = Arc::new(ShareUplinkFirewall::new(runner.clone()));
    let throttle_rx = router.bus().subscribe();
    let throttle = tokio::spawn(run_throttle_consumer(
        throttle_rx,
        Arc::clone(&router),
        Arc::clone(&firewall),
    ));

    // Share-uplink NAT reconcile: apply the persisted share_uplink flag on the
    // active iface at start, then re-apply on every uplink switch so the NAT
    // MASQUERADE rule follows the active uplink. The daemon owns this (the REST
    // share-uplink write path only persists the flag), and the flag is re-read
    // from the agent config on each event so an operator toggle lands without a
    // restart.
    let share_uplink_rx = router.bus().subscribe();
    let share_uplink_task = tokio::spawn(run_share_uplink_consumer(
        share_uplink_rx,
        Arc::clone(&router),
        Arc::clone(&firewall),
        || ados_net::UplinkConfig::load().share_uplink(),
    ));

    // Cellular data-cap tracker: polls sysfs counters at 60 s and publishes
    // `data_cap_threshold` events on the router's bus (consumed by the throttle
    // bridge above). The active-flag writer runs inside the router's own tick.
    // The cap is the OPERATOR's configured cellular limit from the modem
    // sidecar, not the build default — PUT /network/modem persists cap_gb there,
    // so a daemon that ignored it enforced 5 GB regardless of what the operator
    // set. Read it at startup; the poll task below re-reads it so a later change
    // takes effect within one cycle without a restart.
    let modem_cfg_path = std::path::PathBuf::from(ados_net::paths::GS_MODEM_JSON);
    let startup_cap_gb = ModemConfig::load(&modem_cfg_path)
        .cap_gb
        .unwrap_or(ados_net::data_cap::DEFAULT_CAP_GB);
    let data_cap = Arc::new(Mutex::new(
        DataCapTracker::with_config(
            Arc::new(SysfsUsageSource::new()),
            router.bus(),
            startup_cap_gb,
            std::path::PathBuf::from(ados_net::data_cap::USAGE_STATE_PATH),
        )
        .with_emitter(emitter.clone()),
    ));
    // Cellular modem is HW-gated and DISABLED by default: only bring it up when
    // the operator has written the config sidecar AND left `enabled` set. A
    // bare board with no modem config never auto-dials. The data session uses
    // the persisted APN (or "auto"); IMSI-based APN auto-detect over D-Bus is
    // resolved inside the modem manager's bring-up. The REST modem write path
    // only PERSISTS the config file now; the daemon owns the live session, so
    // the poll loop below re-reads the file and reconciles up/down without a
    // restart (the same per-tick-reread contract as the cap).
    let modem_config_present = std::path::Path::new(ados_net::paths::GS_MODEM_JSON).is_file();
    let mut applied_session =
        desired_modem_session(modem_config_present, &ModemConfig::load(&modem_cfg_path));
    match applied_session {
        ModemSession::Up => {
            // Read the live SIM IMSI so carrier-APN auto-detection works on the
            // D-Bus path; the AT fallback reads AT+CIMI itself if D-Bus has none.
            let imsi = modem.read_imsi().await;
            let apn = modem.configured_apn().await;
            let result = modem.bring_up(&apn, imsi.as_deref()).await;
            tracing::info!(imsi_known = imsi.is_some(), result = %result, "modem_bring_up");
        }
        ModemSession::Down | ModemSession::Leave => {
            tracing::info!(
                config = modem_config_present,
                "modem_disabled_by_default_not_dialing"
            );
        }
    }

    let data_cap_task = {
        let data_cap = Arc::clone(&data_cap);
        let modem_cfg_path = modem_cfg_path.clone();
        let modem = Arc::clone(&modem);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(DATA_CAP_INTERVAL);
            let mut applied_cap_gb = startup_cap_gb;
            loop {
                tick.tick().await;
                // Pick up an operator cap change (PUT /network/modem) without a
                // daemon restart. Cheap: a small JSON read once per minute.
                let config_present = modem_cfg_path.is_file();
                let cfg = modem.reload_config().await;
                let cap_gb = cfg.cap_gb.unwrap_or(ados_net::data_cap::DEFAULT_CAP_GB);
                {
                    let mut t = data_cap.lock().await;
                    if cap_gb != applied_cap_gb {
                        t.set_cap(cap_gb);
                        applied_cap_gb = cap_gb;
                    }
                    t.check_month_reset();
                    t.poll_once().await;
                }

                // Reconcile the cellular session to the persisted config. The
                // REST modem write path no longer drives the modem in-process,
                // so an operator enabling/disabling it lands here within one
                // poll. Only act on a transition so a steady state never re-dials.
                let desired = desired_modem_session(config_present, &cfg);
                if desired != applied_session {
                    match desired {
                        ModemSession::Up => {
                            let imsi = modem.read_imsi().await;
                            let apn = modem.configured_apn().await;
                            let result = modem.bring_up(&apn, imsi.as_deref()).await;
                            tracing::info!(result = %result, "modem.reconcile_bring_up");
                        }
                        ModemSession::Down => {
                            let _ = modem.bring_down().await;
                            tracing::info!("modem.reconcile_bring_down");
                        }
                        ModemSession::Leave => {}
                    }
                    applied_session = desired;
                }
            }
        })
    };

    // Operator WiFi-join/forget command socket. The REST `/network/client/*`
    // handlers forward to this when the native daemon owns the uplink, so they
    // never drive `nmcli` on `wlan0` in-process and race this daemon's WiFi
    // manager for the radio. A dedicated manager instance owns the operator
    // actions; at steady state it is idle (holds no lock, touches no nmcli), so
    // it adds no management-link risk, and it shares the `wlan0` advisory file
    // lock + the real system state with the router's WiFi manager so the two
    // never both transition the radio.
    let wifi_cmd = Arc::new(Mutex::new(WifiClientManager::new(runner.clone())));
    let cmdsock_task = {
        let state = ados_net::CmdState {
            wifi: Arc::clone(&wifi_cmd),
        };
        tokio::spawn(async move {
            if let Err(e) = ados_net::cmdsock::serve(state, ados_net::paths::wifi_cmd_sock()).await
            {
                tracing::warn!(error = %e, "wifi command socket exited");
            }
        })
    };

    // Bring up the LAN-side AP and the USB-gadget tether. Both are best-effort:
    // a board with no wlan0 or no configfs logs and continues.
    let mut hostapd = HostapdManager::new(&device_id, None, 6, String::new(), runner.clone());
    hostapd.ensure_passphrase();
    if !hostapd.start().await {
        tracing::warn!(ssid = hostapd.ssid(), "ap_start_incomplete");
    }

    let mut usb_gadget = UsbGadgetManager::new();
    if usb_gadget.configfs_available() {
        if !usb_gadget.setup().await {
            tracing::warn!("usb_gadget_setup_incomplete");
        }
    } else {
        tracing::info!("usb_gadget_configfs_absent_skipping");
    }

    notify_ready();

    // Health loop: tick now, then every HEALTH_INTERVAL, until SIGTERM/SIGINT.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interval = tokio::time::interval(ados_net::health::HEALTH_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                router.tick().await;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("uplink router stopping (SIGINT)");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("uplink router stopping (SIGTERM)");
                break;
            }
        }
    }

    // Graceful shutdown: flush the data-cap counter, bring the modem down (only
    // if it was dialed), tear down the gadget + AP, and stop the background
    // tasks.
    data_cap_task.abort();
    data_cap.lock().await.flush();
    if modem_config_present && modem.enabled().await {
        let _ = modem.bring_down().await;
    }
    usb_gadget.teardown().await;
    hostapd.stop().await;
    throttle.abort();
    share_uplink_task.abort();
    cmdsock_task.abort();

    Ok(())
}
