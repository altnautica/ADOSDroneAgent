//! Tests for the setup-AP guard.

use super::*;
use crate::cmd::testing::ScriptedRunner;
use crate::cmd::CmdOut;

fn hostapd_mgr(dir: &Path, runner: Arc<ScriptedRunner>) -> Arc<Mutex<HostapdManager>> {
    let mut m = HostapdManager::with_paths(
        "58c27faf",
        None,
        6,
        String::new(),
        runner,
        dir.join("hostapd-gs.conf"),
        dir.join("dnsmasq-gs.conf"),
        dir.join("ap-passphrase"),
    );
    // The AP binds an onboard interface resolved by driver; an unresolved
    // one refuses to start, which is not what these cases exercise.
    m.set_interface("wlan0");
    Arc::new(Mutex::new(m))
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
        dir.path().join("hotspot-enabled"),
    );
    guard.reconcile(true).await;

    // The sidecar reflects the stand-down decision (diagnosability).
    let side = read_sidecar(&dir.path().join("ap-guard.json"));
    assert_eq!(side["standing_down"], true);
    assert_eq!(side["reason"], REASON_STANDDOWN);
    assert_eq!(side["wifi_phy_count"], 1);
    assert_eq!(side["client_uplink_active"], true);

    // The AP was proactively torn down: both units stopped + the AP IP freed.
    let calls = hostapd_runner.recorded();
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&"stop".to_string())
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
        dir.path().join("hotspot-enabled"),
    );
    guard.reconcile(true).await;

    let side = read_sidecar(&dir.path().join("ap-guard.json"));
    assert_eq!(side["standing_down"], false);
    assert_eq!(side["reason"], REASON_NO_CLIENT_UPLINK);

    // The AP was brought up (started), never stopped.
    let calls = hostapd_runner.recorded();
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&"start".to_string())
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
        dir.path().join("hotspot-enabled"),
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
        dir.path().join("hotspot-enabled"),
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
        dir.path().join("hotspot-enabled"),
    );

    guard.reconcile(false).await; // stand down
    assert!(guard.stood_down.load(Ordering::SeqCst));
    guard.reconcile(false).await; // restore

    let calls = hostapd_runner.recorded();
    // The last phase started the AP back up.
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&"start".to_string())
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
        dir.path().join("hotspot-enabled"),
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

/// An operator-enabled AP that failed to start is retried every steady-state
/// tick, and an interface that was unset at boot is re-resolved before the
/// retry, so a late-enumerating onboard chip still gets its AP.
#[tokio::test]
async fn ap_guard_retries_start_and_resolves_late_interface() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let phy_dir = make_phys(dir.path(), 1);
    std::fs::write(dir.path().join("hotspot-enabled"), b"").unwrap();

    let wifi_runner = Arc::new(ScriptedRunner::new());
    push_client_disconnected(&wifi_runner);
    push_client_disconnected(&wifi_runner);

    let hostapd_runner = Arc::new(ScriptedRunner::new());
    let inactive = || CmdOut {
        rc: 3,
        stdout: "inactive\n".to_string(),
        stderr: String::new(),
    };
    hostapd_runner.push(inactive()); // tick 1: is_running → inactive
    let hostapd = Arc::new(Mutex::new(HostapdManager::with_paths(
        "58c27faf",
        None,
        6,
        String::new(),
        hostapd_runner.clone(),
        dir.path().join("hostapd-gs.conf"),
        dir.path().join("dnsmasq-gs.conf"),
        dir.path().join("ap-passphrase"),
    )));
    assert!(hostapd.lock().await.interface().is_empty());

    // The onboard chip is absent on the first resolve and present on the next.
    let resolves = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&resolves);
    let guard = SetupApGuard::with_paths(
        Arc::clone(&hostapd),
        wifi_mgr(dir.path(), wifi_runner),
        phy_dir,
        dir.path().join("ap-guard.json"),
        dir.path().join("ap-was-enabled"),
        dir.path().join("hotspot-enabled"),
    )
    .with_resolver(Box::new(move |h: &mut HostapdManager| {
        if seen.fetch_add(1, Ordering::SeqCst) >= 1 {
            h.set_interface("wlan0");
        }
    }));

    guard.reconcile(false).await;
    let started = |calls: &[Vec<String>]| {
        calls.iter().any(|c| {
            c.contains(&"start".to_string()) && c.contains(&"ados-hostapd.service".to_string())
        })
    };
    assert!(
        !started(&hostapd_runner.recorded()),
        "an unresolved interface must not start hostapd"
    );
    assert_eq!(resolves.load(Ordering::SeqCst), 1);

    hostapd_runner.push(inactive()); // tick 2: is_running → inactive
    guard.reconcile(false).await;
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
    assert_eq!(hostapd.lock().await.interface(), "wlan0");
    assert!(
        started(&hostapd_runner.recorded()),
        "the retry did not start hostapd: {:?}",
        hostapd_runner.recorded()
    );
}
