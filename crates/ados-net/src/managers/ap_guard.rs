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
const AP_IFACE: &str = "wlan0";

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
        )
    }

    /// Full constructor (tests inject the phy directory + sidecar + flag paths).
    pub fn with_paths(
        hostapd: Arc<Mutex<HostapdManager>>,
        wifi: WifiClientManager,
        ieee80211_dir: PathBuf,
        sidecar_path: PathBuf,
        ap_was_enabled_flag: PathBuf,
    ) -> Self {
        Self {
            hostapd,
            wifi,
            ieee80211_dir,
            sidecar_path,
            ap_was_enabled_flag,
            stood_down: AtomicBool::new(false),
        }
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
            "ap_interface": AP_IFACE,
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
    /// before this guard existed. Steady-state ticks only restore an AP the guard
    /// itself stood down.
    pub async fn reconcile(&self, startup: bool) {
        let decision = self.evaluate().await;

        if decision.stand_down {
            let hostapd = self.hostapd.lock().await;
            if hostapd.is_running().await {
                info!(
                    wifi_phy_count = decision.wifi_phy_count,
                    ap_interface = AP_IFACE,
                    "ap_setup_standdown: sole radio is a client uplink; bringing the setup AP down"
                );
                hostapd.stop().await;
                hostapd.release_ip().await;
            } else if startup {
                info!(
                    wifi_phy_count = decision.wifi_phy_count,
                    ap_interface = AP_IFACE,
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
        } else if self.stood_down.load(Ordering::SeqCst) && !self.client_join_owns_radio() {
            let mut hostapd = self.hostapd.lock().await;
            if !hostapd.is_running().await {
                hostapd.ensure_passphrase();
                if hostapd.start().await {
                    info!(
                        ap_interface = AP_IFACE,
                        "ap_setup_restored: client uplink gone; bringing the setup AP back up"
                    );
                } else {
                    let ssid = hostapd.ssid().to_string();
                    warn!(ssid = %ssid, "ap_start_incomplete");
                }
            }
        }
        self.stood_down.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;

    fn hostapd_mgr(dir: &Path, runner: Arc<ScriptedRunner>) -> Arc<Mutex<HostapdManager>> {
        Arc::new(Mutex::new(HostapdManager::with_paths(
            "58c27faf",
            None,
            6,
            String::new(),
            runner,
            dir.join("hostapd-gs.conf"),
            dir.join("dnsmasq-gs.conf"),
            dir.join("ap-passphrase"),
        )))
    }

    fn wifi_mgr(dir: &Path, runner: Arc<ScriptedRunner>) -> WifiClientManager {
        WifiClientManager::with_paths(
            "wlan0",
            runner,
            dir.join("ados-wlan0.lock"),
            dir.join("ap-was-enabled"),
            dir.join("ground-station-wifi-client.json"),
        )
    }

    /// Queue the three commands `WifiClientManager::status()` runs, describing a
    /// CONNECTED station (an ACTIVE nmcli row + an assigned IPv4).
    fn push_client_connected(runner: &ScriptedRunner) {
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:HomeWifi:AA\\:BB\\:CC:70:WPA2\n".to_string(),
            stderr: String::new(),
        }); // nmcli wifi list ifname wlan0
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.1.50/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip -4 addr show wlan0
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.1.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip -4 route show default dev wlan0
    }

    /// Queue the three status() commands for a DISCONNECTED station (no ACTIVE
    /// row, no IPv4).
    fn push_client_disconnected(runner: &ScriptedRunner) {
        runner.push(CmdOut {
            rc: 0,
            stdout: String::new(),
            stderr: String::new(),
        }); // nmcli wifi list → no ACTIVE row
        runner.push(CmdOut {
            rc: 0,
            stdout: String::new(),
            stderr: String::new(),
        }); // ip addr → no inet
        runner.push(CmdOut {
            rc: 0,
            stdout: String::new(),
            stderr: String::new(),
        }); // ip route
    }

    fn read_sidecar(path: &Path) -> serde_json::Value {
        let bytes = std::fs::read(path).expect("sidecar written");
        serde_json::from_slice(&bytes).expect("sidecar is valid json")
    }

    /// Make `dir/ieee80211` hold `n` phy entries (phy0..phy{n-1}).
    fn make_phys(dir: &Path, n: usize) -> PathBuf {
        let phy_dir = dir.join("ieee80211");
        std::fs::create_dir_all(&phy_dir).unwrap();
        for i in 0..n {
            std::fs::create_dir_all(phy_dir.join(format!("phy{i}"))).unwrap();
        }
        phy_dir
    }

    #[test]
    fn decide_stands_down_only_for_single_radio_with_client_uplink() {
        // The one firing case.
        assert!(decide(1, true).stand_down);
        assert_eq!(decide(1, true).reason, REASON_STANDDOWN);
        // Single radio, no client uplink → the AP is the lifeline, keep it.
        assert!(!decide(1, false).stand_down);
        assert_eq!(decide(1, false).reason, REASON_NO_CLIENT_UPLINK);
        // Multiple radios → the AP can sit on a second radio; never fire.
        assert!(!decide(2, true).stand_down);
        assert_eq!(decide(2, true).reason, REASON_NOT_SINGLE_RADIO);
        assert!(!decide(2, false).stand_down);
        // No wifi radio at all → never fire.
        assert!(!decide(0, true).stand_down);
        assert_eq!(decide(0, true).reason, REASON_NOT_SINGLE_RADIO);
        assert!(!decide(0, false).stand_down);
    }

    #[test]
    fn count_wifi_phys_counts_entries_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        // Absent directory → 0 (conservative: guard cannot confirm single radio).
        assert_eq!(count_wifi_phys(&dir.path().join("ieee80211")), 0);
        // One phy → 1.
        let one = make_phys(dir.path(), 1);
        assert_eq!(count_wifi_phys(&one), 1);
        // Two phys → 2.
        let dir2 = tempfile::tempdir().unwrap();
        let two = make_phys(dir2.path(), 2);
        assert_eq!(count_wifi_phys(&two), 2);
    }

    #[tokio::test]
    async fn single_radio_with_client_uplink_brings_a_running_ap_down_and_releases_the_ip() {
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 1);

        let wifi_runner = Arc::new(ScriptedRunner::new());
        push_client_connected(&wifi_runner); // is_up() → connected

        let hostapd_runner = Arc::new(ScriptedRunner::new());
        hostapd_runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is_running() → active
            // stop() issues two systemctl stops; release_ip() one ip addr del.
            // (unmatched → default rc0, but we push explicit ok for clarity)
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl stop ados-dnsmasq-gs
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl stop ados-hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr del 192.168.4.1/24

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );
        guard.reconcile(true).await;

        // The sidecar reflects the stand-down decision (Rule 44 diagnosability).
        let side = read_sidecar(&dir.path().join("ap-guard.json"));
        assert_eq!(side["standing_down"], true);
        assert_eq!(side["reason"], REASON_STANDDOWN);
        assert_eq!(side["wifi_phy_count"], 1);
        assert_eq!(side["client_uplink_active"], true);

        // The AP was proactively torn down: both units stopped + the AP IP freed.
        let calls = hostapd_runner.recorded();
        assert!(
            calls.iter().any(|c| c.contains(&"stop".to_string())
                && c.contains(&"ados-hostapd.service".to_string())),
            "hostapd was not stopped: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("ip")
                    && c.contains(&"del".to_string())
                    && c.contains(&"192.168.4.1/24".to_string())),
            "the AP IP was not released: {calls:?}"
        );
        // It never tried to START the AP.
        assert!(
            !calls.iter().any(|c| c.contains(&"start".to_string())),
            "the guard started the AP while standing down: {calls:?}"
        );
    }

    #[tokio::test]
    async fn fresh_single_radio_with_no_uplink_keeps_the_setup_ap_lifeline() {
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 1);

        let wifi_runner = Arc::new(ScriptedRunner::new());
        push_client_disconnected(&wifi_runner); // is_up() → not connected

        // Startup !stand_down brings the AP up exactly as before: write_config
        // (no runner), assign_ip (2 calls), systemctl start hostapd + dnsmasq.
        let hostapd_runner = Arc::new(ScriptedRunner::new());
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr add
        hostapd_runner.push(CmdOut::failed(0, "")); // ip link set up
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start ados-hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start ados-dnsmasq-gs

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );
        guard.reconcile(true).await;

        let side = read_sidecar(&dir.path().join("ap-guard.json"));
        assert_eq!(side["standing_down"], false);
        assert_eq!(side["reason"], REASON_NO_CLIENT_UPLINK);

        // The AP was brought up (started), never stopped.
        let calls = hostapd_runner.recorded();
        assert!(
            calls.iter().any(|c| c.contains(&"start".to_string())
                && c.contains(&"ados-hostapd.service".to_string())),
            "the setup AP was not started for a fresh single-radio GS: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains(&"stop".to_string())),
            "the fresh-headless AP was stopped: {calls:?}"
        );
    }

    #[tokio::test]
    async fn multi_radio_with_client_uplink_does_not_stand_down() {
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 2); // two radios

        let wifi_runner = Arc::new(ScriptedRunner::new());
        push_client_connected(&wifi_runner); // client uplink IS active

        // Startup with !stand_down brings the AP up (a second radio hosts it).
        let hostapd_runner = Arc::new(ScriptedRunner::new());
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr add
        hostapd_runner.push(CmdOut::failed(0, "")); // ip link set up
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start ados-hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start ados-dnsmasq-gs

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );
        guard.reconcile(true).await;

        let side = read_sidecar(&dir.path().join("ap-guard.json"));
        assert_eq!(side["standing_down"], false);
        assert_eq!(side["reason"], REASON_NOT_SINGLE_RADIO);
        // The AP came up; it was NOT stood down.
        let calls = hostapd_runner.recorded();
        assert!(calls.iter().any(|c| c.contains(&"start".to_string())));
        assert!(!calls.iter().any(|c| c.contains(&"stop".to_string())));
    }

    #[tokio::test]
    async fn steady_state_does_not_bring_up_an_ap_the_guard_never_stood_down() {
        // A steady reconcile (startup=false) with no client uplink and no prior
        // guard stand-down must be a pure no-op on the AP: it preserves today's
        // behavior (the AP is whatever the boot / join-leave path left it).
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 1);

        let wifi_runner = Arc::new(ScriptedRunner::new());
        push_client_disconnected(&wifi_runner);

        let hostapd_runner = Arc::new(ScriptedRunner::new());

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );
        guard.reconcile(false).await;

        // No AP command was issued at all.
        assert!(
            hostapd_runner.recorded().is_empty(),
            "steady reconcile touched the AP: {:?}",
            hostapd_runner.recorded()
        );
    }

    #[tokio::test]
    async fn restores_only_after_the_guard_stood_the_ap_down_and_no_join_owns_the_radio() {
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 1);

        // Tick 1: client uplink up → the guard stands the running AP down.
        let wifi_runner = Arc::new(ScriptedRunner::new());
        push_client_connected(&wifi_runner);
        let hostapd_runner = Arc::new(ScriptedRunner::new());
        hostapd_runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is_running → active
        hostapd_runner.push(CmdOut::failed(0, "")); // stop dnsmasq
        hostapd_runner.push(CmdOut::failed(0, "")); // stop hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr del
                                                    // Tick 2: client uplink gone → restore the AP we stood down.
        push_client_disconnected(&wifi_runner);
        hostapd_runner.push(CmdOut {
            rc: 3,
            stdout: "inactive\n".to_string(),
            stderr: String::new(),
        }); // is_running → inactive
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr add
        hostapd_runner.push(CmdOut::failed(0, "")); // ip link set up
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // systemctl start dnsmasq

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );

        guard.reconcile(false).await; // stand down
        assert!(guard.stood_down.load(Ordering::SeqCst));
        guard.reconcile(false).await; // restore

        let calls = hostapd_runner.recorded();
        // The last phase started the AP back up.
        assert!(
            calls.iter().any(|c| c.contains(&"start".to_string())
                && c.contains(&"ados-hostapd.service".to_string())),
            "the guard did not restore the AP it stood down: {calls:?}"
        );
        assert!(!guard.stood_down.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn does_not_restore_the_ap_while_a_client_join_owns_the_radio() {
        let dir = tempfile::tempdir().unwrap();
        let phy_dir = make_phys(dir.path(), 1);

        let wifi_runner = Arc::new(ScriptedRunner::new());
        // Two ticks worth of disconnected status probes.
        push_client_connected(&wifi_runner); // tick1: client up → stand down
        push_client_disconnected(&wifi_runner); // tick2: client down

        let hostapd_runner = Arc::new(ScriptedRunner::new());
        hostapd_runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // tick1 is_running → active
        hostapd_runner.push(CmdOut::failed(0, "")); // stop dnsmasq
        hostapd_runner.push(CmdOut::failed(0, "")); // stop hostapd
        hostapd_runner.push(CmdOut::failed(0, "")); // ip addr del

        let guard = SetupApGuard::with_paths(
            hostapd_mgr(dir.path(), hostapd_runner.clone()),
            wifi_mgr(dir.path(), wifi_runner),
            phy_dir,
            dir.path().join("ap-guard.json"),
            dir.path().join("ap-was-enabled"),
        );

        guard.reconcile(false).await; // stand down (guard_stood_down = true)

        // A client join now owns the radio (the wifi-client manager wrote the
        // handoff flag). The next tick must NOT bring the AP up (leave() will).
        std::fs::write(dir.path().join("ap-was-enabled"), "0\n").unwrap();
        guard.reconcile(false).await;

        let calls = hostapd_runner.recorded();
        assert!(
            !calls.iter().any(|c| c.contains(&"start".to_string())),
            "the guard restored the AP while a client join owned the radio: {calls:?}"
        );
    }
}
