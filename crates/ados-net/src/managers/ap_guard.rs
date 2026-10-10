//! Setup-AP stand-down guard for the single-radio ground station.
//!
//! On a box with exactly one wifi-capable phy, the onboard radio cannot serve
//! the setup access point (`192.168.4.1` on `wlan0`) and a wifi-CLIENT uplink at
//! the same time: one radio cannot sustain concurrent AP + station mode, so
//! under load the client link collapses and the box drops off the LAN
//! (observed: `wlan0` held both `192.168.4.1` and the LAN client IP, then the
//! client dropped while the AP stayed up). The setup AP is a first-boot lifeline
//! for a box with NO other reachability; it is redundant AND harmful once that
//! sole radio is already carrying a working client uplink.
//!
//! This guard decides whether the setup AP should stand down and reconciles the
//! live AP against that decision each health tick. It fires ONLY when BOTH hold:
//!   (a) exactly one wifi phy exists on the box, AND
//!   (b) that radio carries an active wifi-client uplink (the uplink manager
//!       reports it connected: associated + has an IPv4).
//!
//! Every other case leaves today's behavior untouched: multiple radios (the AP
//! can sit on a second radio), no wifi at all, or no working client uplink (a
//! fresh headless GS whose only reachability is the setup AP keeps it). The
//! guard only ever brings back up an AP it itself stood down, and never while a
//! client join owns the radio, so the join/leave path and the fresh-headless
//! case behave exactly as before. The decision is written to a sidecar and
//! logged so it is diagnosable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::managers::{HostapdManager, WifiClientManager};
use crate::router::UplinkManager;

/// The mac80211 / cfg80211 registry. One entry per registered wiphy (`phy0`,
/// `phy1`, ...); counting entries here is the wifi-radio count.
const IEEE80211_DIR: &str = "/sys/class/ieee80211";

/// The AP interface the hostapd manager binds. The client-uplink probe checks
/// this same interface (they contend for it on a single radio).
///
/// Resolved by driver rather than assumed to be `wlan0`: interface names race
/// at boot, and reporting a name the AP is not actually on is a status surface
/// stating something untrue. The hostapd manager resolves the same way from the
/// same inputs, so the two agree without this holding a lock on it. `None`
/// when no onboard WiFi resolves; the sidecar then reports `null`.
fn ap_interface() -> Option<String> {
    ados_protocol::netif::resolve_ap_interface(
        &ados_protocol::ap_country::configured_ap_interface(),
        ados_protocol::netif::radio_interface().as_deref(),
    )
    .ok()
}

/// Reason strings surfaced on the guard sidecar / status for diagnosability.
pub const REASON_STANDDOWN: &str = "single_radio_client_uplink";
pub const REASON_NOT_SINGLE_RADIO: &str = "not_single_radio";
pub const REASON_NO_CLIENT_UPLINK: &str = "no_client_uplink";

/// The stand-down decision for the setup AP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApGuardDecision {
    /// True when the AP must stand down (both guard conditions hold).
    pub stand_down: bool,
    /// The number of wifi-capable phys on the box at decision time.
    pub wifi_phy_count: usize,
    /// Whether the AP interface carries an active wifi-client uplink.
    pub client_uplink_active: bool,
    /// A stable reason string for the sidecar / status.
    pub reason: &'static str,
}

/// The pure decision: stand down only when there is exactly one wifi radio AND
/// it is already carrying a client uplink. No IO — every other case preserves
/// today's behavior (AP untouched).
pub fn decide(wifi_phy_count: usize, client_uplink_active: bool) -> ApGuardDecision {
    let stand_down = wifi_phy_count == 1 && client_uplink_active;
    let reason = if stand_down {
        REASON_STANDDOWN
    } else if wifi_phy_count != 1 {
        REASON_NOT_SINGLE_RADIO
    } else {
        REASON_NO_CLIENT_UPLINK
    };
    ApGuardDecision {
        stand_down,
        wifi_phy_count,
        client_uplink_active,
        reason,
    }
}

/// Count the wifi-capable phys on the box by enumerating `/sys/class/ieee80211/`
/// (one entry per registered wiphy). A missing / unreadable directory — a board
/// with no wifi, or a dev host — counts as zero, so the guard never fires when it
/// cannot confirm the single-radio premise.
pub fn count_wifi_phys(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(Result::ok).count(),
        Err(_) => 0,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The operator's setup-AP enable marker (`network.hotspot.enabled`, mirrored
/// by the installer). While it exists the AP is wanted, so a failed start is
/// retried every health tick.
const HOTSPOT_ENABLED_FLAG: &str = "/etc/ados/hotspot-enabled";

/// Re-resolves the AP interface on a manager whose interface is still unset
/// (the onboard chip was not enumerated yet when the daemon started).
pub type ApIfaceResolver = Box<dyn Fn(&mut HostapdManager) + Send + Sync>;

fn resolve_live(hostapd: &mut HostapdManager) {
    hostapd.resolve_interface(&ados_protocol::ap_country::configured_ap_interface());
}

/// Reconciles the setup AP against the single-radio-plus-client-uplink guard.
pub struct SetupApGuard {
    /// The live hostapd manager, shared with the daemon + the operator command
    /// socket so the guard drives the same AP instance (not a parallel owner).
    hostapd: Arc<Mutex<HostapdManager>>,
    /// A read-only wifi-client manager used only to probe `is_up()` (the uplink
    /// manager's "wlan0 is an active client uplink" signal). It queries the live
    /// system and holds no lock, so it never contends with the router's manager.
    wifi: WifiClientManager,
    ieee80211_dir: PathBuf,
    sidecar_path: PathBuf,
    ap_was_enabled_flag: PathBuf,
    hotspot_enabled_flag: PathBuf,
    resolver: ApIfaceResolver,
    /// Whether the guard itself stood the AP down. The reconcile only ever
    /// restores an AP it took down, so the fresh-headless and operator
    /// join/leave paths are never touched.
    stood_down: AtomicBool,
}

impl SetupApGuard {
    /// Guard with canonical paths.
    pub fn new(hostapd: Arc<Mutex<HostapdManager>>, wifi: WifiClientManager) -> Self {
        Self::with_paths(
            hostapd,
            wifi,
            PathBuf::from(IEEE80211_DIR),
            PathBuf::from(crate::paths::AP_GUARD_JSON),
            PathBuf::from(crate::paths::AP_WAS_ENABLED_FLAG),
            PathBuf::from(HOTSPOT_ENABLED_FLAG),
        )
    }

    /// Full constructor (tests inject the phy directory + sidecar + flag paths).
    pub fn with_paths(
        hostapd: Arc<Mutex<HostapdManager>>,
        wifi: WifiClientManager,
        ieee80211_dir: PathBuf,
        sidecar_path: PathBuf,
        ap_was_enabled_flag: PathBuf,
        hotspot_enabled_flag: PathBuf,
    ) -> Self {
        Self {
            hostapd,
            wifi,
            ieee80211_dir,
            sidecar_path,
            ap_was_enabled_flag,
            hotspot_enabled_flag,
            resolver: Box::new(resolve_live),
            stood_down: AtomicBool::new(false),
        }
    }

    /// Replace the late interface resolver (tests stand in for sysfs).
    pub fn with_resolver(mut self, resolver: ApIfaceResolver) -> Self {
        self.resolver = resolver;
        self
    }

    /// True while a client-join owns the radio: the wifi-client manager writes
    /// the `ap-was-enabled` handoff flag when it stops the AP to take `wlan0` for
    /// a station connection and clears it on `leave`. While the flag is present
    /// the join/leave path owns AP restoration, so the guard must not race it by
    /// bringing the AP back up.
    fn client_join_owns_radio(&self) -> bool {
        self.ap_was_enabled_flag.exists()
    }

    /// Compute the live decision (phy count + client-uplink probe) and record it
    /// to the sidecar. Split out so a caller can inspect the decision in tests.
    pub async fn evaluate(&self) -> ApGuardDecision {
        let phys = count_wifi_phys(&self.ieee80211_dir);
        let client_up = self.wifi.is_up().await;
        let decision = decide(phys, client_up);
        self.write_sidecar(&decision);
        decision
    }

    fn write_sidecar(&self, d: &ApGuardDecision) {
        let body = json!({
            "standing_down": d.stand_down,
            "reason": d.reason,
            "wifi_phy_count": d.wifi_phy_count,
            "client_uplink_active": d.client_uplink_active,
            "ap_interface": ap_interface(),
            "updated_ms": now_ms(),
        });
        match serde_json::to_vec(&body) {
            Ok(bytes) => {
                if let Err(exc) = crate::sidecar::write_atomic(&self.sidecar_path, &bytes) {
                    warn!(error = %exc, "ap_guard_sidecar_write_failed");
                }
            }
            Err(exc) => warn!(error = %exc, "ap_guard_sidecar_encode_failed"),
        }
    }

    /// One reconcile pass. `startup` = the boot-time call, which brings the AP up
    /// in the non-stand-down case exactly as the daemon did unconditionally
    /// before this guard existed. Steady-state ticks restore an AP the guard
    /// itself stood down, and retry an operator-enabled AP that is not running
    /// (a failed boot start, or an onboard chip that enumerated late), with no
    /// cap: every tick re-resolves an unset interface and starts again.
    pub async fn reconcile(&self, startup: bool) {
        let decision = self.evaluate().await;

        if decision.stand_down {
            let hostapd = self.hostapd.lock().await;
            if hostapd.is_running().await {
                info!(
                    wifi_phy_count = decision.wifi_phy_count,
                    ap_interface = ?ap_interface(),
                    "ap_setup_standdown: sole radio is a client uplink; bringing the setup AP down"
                );
                hostapd.stop().await;
                hostapd.release_ip().await;
            } else if startup {
                info!(
                    wifi_phy_count = decision.wifi_phy_count,
                    ap_interface = ?ap_interface(),
                    "ap_setup_standdown: sole radio is a client uplink; leaving the setup AP down"
                );
            }
            drop(hostapd);
            self.stood_down.store(true, Ordering::SeqCst);
            return;
        }

        // The AP is allowed. On startup, bring it up unconditionally (today's
        // behavior). At steady state, only restore an AP the guard itself stood
        // down, and only when no client join owns the radio.
        if startup {
            let mut hostapd = self.hostapd.lock().await;
            hostapd.ensure_passphrase();
            if !hostapd.start().await {
                let ssid = hostapd.ssid().to_string();
                warn!(ssid = %ssid, "ap_start_incomplete");
            }
        } else if !self.client_join_owns_radio() {
            let restoring = self.stood_down.load(Ordering::SeqCst);
            if restoring || self.hotspot_enabled_flag.exists() {
                let mut hostapd = self.hostapd.lock().await;
                if !hostapd.is_running().await {
                    if hostapd.interface().is_empty() {
                        (self.resolver)(&mut hostapd);
                    }
                    hostapd.ensure_passphrase();
                    if hostapd.start().await {
                        info!(
                            ap_interface = %hostapd.interface(),
                            restoring,
                            "ap_setup_started: bringing the setup AP up"
                        );
                    } else {
                        let ssid = hostapd.ssid().to_string();
                        warn!(ssid = %ssid, "ap_start_incomplete_retrying_next_tick");
                    }
                }
            }
        }
        self.stood_down.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[path = "ap_guard_tests.rs"]
mod tests;
