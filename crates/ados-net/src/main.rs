//! `ados-net` daemon — the ground-station uplink matrix.
//!
//! Wires the full uplink stack:
//!   - the priority-failover router FSM over the real ethernet + Wi-Fi-client
//!     managers (the HW-gated cellular slot stays a stub until the modem chunk),
//!   - the active-uplink sidecar (written inside the router's tick/switch),
//!   - the cellular data-cap tracker polling at 60 s, publishing threshold
//!     events on the router's bus,
//!   - the firewall reconciler that turns the share-uplink flag, the active
//!     uplink and the cap level into one consistent NAT + shaping state,
//!   - the cellular session reconciler, which re-dials on a fixed cadence
//!     whenever the operator wants the modem up and the session is not,
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
    ModemSession, SetupApGuard, UsbGadgetManager, WifiClientManager,
};
use ados_net::router::failover;
use ados_net::sysfs::detect_ethernet_iface;
use ados_net::{run_firewall_reconciler, ShareUplinkFirewall, UplinkManager, UplinkRouter};

/// Cadence of the cellular session reconcile: how soon a failed dial or a
/// dropped session is retried.
const MODEM_RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

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
    let device_id = ados_protocol::identity::device_id(None);
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

    // Cellular data-cap tracker: polls sysfs counters at 60 s and publishes
    // `data_cap_threshold` events on the router's bus (consumed by the firewall
    // reconciler below). The cap is the OPERATOR's configured cellular limit
    // from the modem sidecar; the poll task re-reads it so a later change takes
    // effect within one cycle without a restart.
    let modem_cfg_path = std::path::PathBuf::from(ados_net::paths::GS_MODEM_JSON);
    let startup_cap_gb = ModemConfig::load(&modem_cfg_path)
        .cap_gb
        .unwrap_or(ados_net::data_cap::DEFAULT_CAP_GB);
    let tracker = DataCapTracker::with_config(
        Arc::new(SysfsUsageSource::new()),
        router.bus(),
        startup_cap_gb,
        std::path::PathBuf::from(ados_net::data_cap::USAGE_STATE_PATH),
    )
    .with_emitter(emitter.clone());
    let initial_cap = tracker.classify();
    let data_cap = Arc::new(Mutex::new(tracker));

    // Firewall reconciler: one serialized owner of NAT, ip_forward and the
    // cellular shaping, driven by the share-uplink flag (re-read from the agent
    // config each pass), the router's active iface, the modem's metered iface
    // and the cap level. Subscribe BEFORE spawning so an event published right
    // after the spawn is not lost to the broadcast channel.
    let firewall = Arc::new(ShareUplinkFirewall::new(runner.clone()));
    let firewall_rx = router.bus().subscribe();
    let firewall_modem = Arc::clone(&modem);
    let firewall_task = tokio::spawn(run_firewall_reconciler(
        firewall_rx,
        Arc::clone(&router),
        Arc::clone(&firewall),
        || ados_net::UplinkConfig::load().share_uplink(),
        move || firewall_modem.cellular_iface(),
        initial_cap,
    ));

    let data_cap_task = {
        let data_cap = Arc::clone(&data_cap);
        let modem_cfg_path = modem_cfg_path.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(DATA_CAP_INTERVAL);
            let mut applied_cap_gb = startup_cap_gb;
            loop {
                tick.tick().await;
                // Pick up an operator cap change (PUT /network/modem) without a
                // daemon restart. Cheap: a small JSON read once per minute.
                let cap_gb = ModemConfig::load(&modem_cfg_path)
                    .cap_gb
                    .unwrap_or(ados_net::data_cap::DEFAULT_CAP_GB);
                let mut t = data_cap.lock().await;
                if cap_gb != applied_cap_gb {
                    t.set_cap(cap_gb);
                    applied_cap_gb = cap_gb;
                }
                t.check_month_reset();
                t.poll_once().await;
            }
        })
    };

    // Cellular session reconcile. The modem is HW-gated and DISABLED by
    // default: it is only dialed when the operator has written the config
    // sidecar and left `enabled` set, so a bare board never auto-dials. The
    // REST modem write path only PERSISTS the config; this loop re-reads it and
    // is level-triggered: while the operator wants the session up and it is
    // not (a failed dial, a SIM not yet registered, a carrier detach, a USB
    // re-enumeration), it dials again every pass. A disable tears down once.
    let modem_task = {
        let modem = Arc::clone(&modem);
        let modem_cfg_path = modem_cfg_path.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(MODEM_RECONCILE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut torn_down = false;
            let mut last_connected: Option<bool> = None;
            loop {
                tick.tick().await;
                let config_present = modem_cfg_path.is_file();
                let cfg = modem.reload_config().await;
                match desired_modem_session(config_present, &cfg) {
                    ModemSession::Up => {
                        torn_down = false;
                        if modem.session_up().await {
                            continue;
                        }
                        // Read the live SIM IMSI so carrier-APN auto-detection
                        // works on the D-Bus path; the AT fallback reads
                        // AT+CIMI itself if D-Bus has none.
                        let imsi = modem.read_imsi().await;
                        let apn = modem.configured_apn().await;
                        let result = modem.bring_up(&apn, imsi.as_deref()).await;
                        let connected = result
                            .get("connected")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        if last_connected != Some(connected) {
                            tracing::info!(result = %result, "modem.reconcile_bring_up");
                        } else {
                            tracing::debug!(result = %result, "modem.reconcile_bring_up");
                        }
                        last_connected = Some(connected);
                    }
                    ModemSession::Down => {
                        if !torn_down {
                            let _ = modem.bring_down().await;
                            tracing::info!("modem.reconcile_bring_down");
                            torn_down = true;
                            last_connected = None;
                        }
                    }
                    ModemSession::Leave => {}
                }
            }
        })
    };

    // Bring up the LAN-side AP and the USB-gadget tether. Both are best-effort:
    // a board with no wlan0 or no configfs logs and continues. The AP manager is
    // shared (behind a Mutex) with the operator command socket below so the AP
    // PUT write drives this same live instance instead of a second hostapd owner.
    // The operator's `network.hotspot.password`, when they have set one. It was
    // passed as an empty string, so the "a configured passphrase wins" branch
    // was unreachable from the path that actually runs: a fleet that set one
    // shared credential got a different generated value on every box instead.
    let configured_ap_passphrase = ados_protocol::ap_country::configured_hotspot_password();
    let hostapd = Arc::new(Mutex::new(HostapdManager::new(
        &device_id,
        None,
        6,
        configured_ap_passphrase,
        runner.clone(),
    )));
    hostapd.lock().await.ensure_passphrase();

    // The setup-AP guard reconciles the LAN-side AP against the single-radio +
    // client-uplink collision: on a box whose sole wifi radio is already
    // carrying a client uplink, the setup AP and the client cannot share the one
    // radio, so the guard stands the AP down (and restores it when that uplink is
    // gone). In every other case — multiple radios, no wifi, or no working
    // client uplink — the AP comes up exactly as before, so a fresh headless GS
    // whose only reachability is the setup AP keeps it. The initial reconcile
    // replaces the old unconditional AP bring-up.
    let ap_guard = SetupApGuard::new(Arc::clone(&hostapd), WifiClientManager::new(runner.clone()));
    ap_guard.reconcile(true).await;

    // Operator uplink-matrix command socket. The REST `/network/*` write handlers
    // forward to this when the native daemon owns the uplink, so they never drive
    // `nmcli` / `hostapd` / `iptables` in-process and race the daemon's managers
    // for the radio + firewall. A dedicated WiFi-client instance owns the operator
    // join/forget/autoconnect actions; at steady state it is idle (holds no lock,
    // touches no nmcli), so it adds no management-link risk, and it shares the
    // `wlan0` advisory file lock + the real system state with the router's WiFi
    // manager so the two never both transition the radio. The AP / ethernet /
    // modem managers shared here are the same live instances the daemon already
    // owns (the modem) or a stateless-apply peer (ethernet), so the socket drives
    // the live system, not a parallel copy. The share-uplink op drives the same
    // firewall the uplink-switch consumer does, which serializes its applies, so
    // an operator toggle takes effect now instead of at the next uplink switch.
    let wifi_cmd = Arc::new(Mutex::new(WifiClientManager::new(runner.clone())));
    let eth_iface = detect_ethernet_iface();
    let eth_cmd = Arc::new(EthernetManager::new(eth_iface, runner.clone()));
    let cmdsock_task = {
        let state = ados_net::CmdState {
            wifi: Arc::clone(&wifi_cmd),
            hostapd: Arc::clone(&hostapd),
            ethernet: Arc::clone(&eth_cmd),
            modem: Arc::clone(&modem),
            firewall: Arc::clone(&firewall),
            router: Arc::clone(&router),
        };
        tokio::spawn(async move {
            if let Err(e) = ados_net::cmdsock::serve(state, ados_net::paths::wifi_cmd_sock()).await
            {
                tracing::warn!(error = %e, "wifi command socket exited");
            }
        })
    };

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
                // Reconcile the setup AP against the single-radio + client-uplink
                // guard so a client link that comes up (or drops) after boot is
                // followed within one health cycle.
                ap_guard.reconcile(false).await;
                // Then prove the AP the guard just decided to keep is actually
                // on the air. `systemctl is-active` only says a process exists;
                // this asks the radio and restarts hostapd when the phy is not
                // serving the BSS. A probe that cannot be made never acts.
                hostapd.lock().await.supervise().await;
                // Keep the tethered host's DHCP server alive on usb0.
                usb_gadget.supervise().await;
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
    modem_task.abort();
    data_cap.lock().await.flush();
    if modem.session_up().await {
        let _ = modem.bring_down().await;
    }
    usb_gadget.teardown().await;
    hostapd.lock().await.stop().await;
    firewall_task.abort();
    cmdsock_task.abort();

    Ok(())
}
