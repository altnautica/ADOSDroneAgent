use super::*;
use crate::cmd::testing::ScriptedRunner;
use crate::cmd::CmdOut;
use serde_json::json;

fn mgr(dir: &std::path::Path, runner: Arc<ScriptedRunner>) -> WifiClientManager {
    WifiClientManager::with_paths(
        "wlan0",
        runner,
        dir.join(lock_file_name("wlan0")),
        dir.join("ap-was-enabled"),
        dir.join("ground-station-wifi-client.json"),
    )
}

/// A sysfs `/sys/class/net` stand-in: each entry gets a `phy80211` marker
/// (what `is_wireless_in` reads) and a `device/driver` symlink whose target
/// name is the driver.
fn fake_net_root(dir: &std::path::Path, ifaces: &[(&str, &str)]) -> std::path::PathBuf {
    let root = dir.join("class-net");
    for (name, driver) in ifaces {
        let iface = root.join(name);
        std::fs::create_dir_all(iface.join("phy80211")).unwrap();
        std::fs::create_dir_all(iface.join("device")).unwrap();
        let target = dir.join("drivers").join(driver);
        std::fs::create_dir_all(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, iface.join("device").join("driver")).unwrap();
    }
    root
}

#[test]
fn station_resolves_past_a_radio_that_enumerated_first() {
    // The inversion that motivated the change: `wlan0` is the USB injection
    // radio this boot and the onboard chip came up as `wlan1`. A name-bound
    // station would run nmcli on the aircraft's flight link.
    let dir = tempfile::tempdir().unwrap();
    let root = fake_net_root(dir.path(), &[("wlan0", "rtl88x2eu"), ("wlan1", "brcmfmac")]);
    let ifaces = ados_protocol::netif::list_wireless_in(&root);
    assert_eq!(ifaces.first().map(|c| c.name.as_str()), Some("wlan0"));
    assert_eq!(
        station_iface_from(&ifaces, "", None).as_deref(),
        Some("wlan1")
    );
    // The lock is named for the radio actually held, so hostapd's holder and
    // this one collide on the same file instead of passing each other by.
    assert_eq!(
        lock_path_for(&station_iface_from(&ifaces, "", None).unwrap()),
        std::path::PathBuf::from("/var/lock/ados-wlan1.lock")
    );
}

#[test]
fn no_station_resolves_when_only_the_flight_radio_is_present() {
    // Radio-only box: no onboard management chip to hand the station, so
    // there is no station interface rather than a guessed `wlan0`.
    let dir = tempfile::tempdir().unwrap();
    let root = fake_net_root(dir.path(), &[("wlan0", "rtl88x2eu")]);
    let ifaces = ados_protocol::netif::list_wireless_in(&root);
    assert_eq!(station_iface_from(&ifaces, "", None), None);
}

#[tokio::test]
async fn join_refuses_with_no_station_interface() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    let mut m = mgr(dir.path(), Arc::clone(&runner));
    m.interface.clear();
    let res = m.join("SomeAP", Some("password1"), true).await;
    assert_eq!(res["error"], "no_station_interface");
    assert!(runner.recorded().is_empty(), "nothing ran");
}

fn active() -> CmdOut {
    CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    }
}

fn inactive() -> CmdOut {
    CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn hostapd_starts(runner: &ScriptedRunner) -> usize {
    runner
        .recorded()
        .iter()
        .filter(|c| c.iter().any(|a| a == "start") && c.iter().any(|a| a == HOSTAPD_UNIT))
        .count()
}

#[tokio::test]
async fn switching_networks_then_leaving_restores_the_ap() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    // First join takes the radio from a running AP (forced).
    runner.push(active()); // is-active
    runner.push(CmdOut::failed(0, "")); // systemctl stop hostapd
    runner.push(CmdOut::failed(0, "")); // nmcli connect ok
    let mut m = mgr(dir.path(), Arc::clone(&runner));
    assert_eq!(m.join("NetA", None, true).await["joined"], true);
    assert!(m.read_ap_flag());
    // Second join (switching SSIDs): hostapd is down now.
    runner.push(inactive()); // is-active
    runner.push(CmdOut::failed(0, "")); // nmcli connect ok
    assert_eq!(m.join("NetB", None, false).await["joined"], true);
    assert!(
        m.read_ap_flag(),
        "the first join's record survives the rejoin"
    );
    // Leaving restores the AP.
    let before = hostapd_starts(&runner);
    m.leave().await;
    assert_eq!(hostapd_starts(&runner), before + 1);
    assert!(!m.holds_lock());
}

#[tokio::test]
async fn leaving_after_the_network_dropped_still_restores_the_ap() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(active()); // is-active
    runner.push(CmdOut::failed(0, "")); // systemctl stop hostapd
    runner.push(CmdOut::failed(0, "")); // nmcli connect ok
    let mut m = mgr(dir.path(), Arc::clone(&runner));
    assert_eq!(m.join("NetA", None, true).await["joined"], true);
    // The joined network went away: status reports no active SSID (the
    // scripted runner answers every status query with empty output).
    let before = hostapd_starts(&runner);
    let res = m.leave().await;
    assert_eq!(res["left"], false);
    assert_eq!(hostapd_starts(&runner), before + 1);
    assert!(!m.read_ap_flag());
    assert!(!m.holds_lock());
}

#[test]
fn station_refuses_to_take_the_interface_the_radio_reports() {
    // The radio's own published verdict outranks classification: even when
    // the driver table would accept `wlan1`, a radio claiming it means the
    // station must not touch it.
    let dir = tempfile::tempdir().unwrap();
    let root = fake_net_root(dir.path(), &[("wlan0", "brcmfmac"), ("wlan1", "brcmfmac")]);
    let ifaces = ados_protocol::netif::list_wireless_in(&root);
    assert_eq!(
        station_iface_from(&ifaces, "", Some("wlan0")).as_deref(),
        Some("wlan1")
    );
}

#[test]
fn flock_is_held_in_struct_and_blocks_a_second_holder() {
    let dir = tempfile::tempdir().unwrap();
    let r1 = Arc::new(ScriptedRunner::new());
    let mut m1 = mgr(dir.path(), r1);
    // Acquire on m1 → held.
    assert!(m1.acquire_lock());
    assert!(m1.holds_lock());

    // A second manager pointed at the SAME lock path cannot acquire while
    // m1 holds it. (Separate fd → flock contention is observed.)
    let r2 = Arc::new(ScriptedRunner::new());
    let mut m2 = mgr(dir.path(), r2);
    // Point m2 at m1's exact lock file.
    m2.lock_path = dir.path().join(lock_file_name("wlan0"));
    assert!(!m2.acquire_lock(), "second holder must fail while m1 holds");
    assert!(!m2.holds_lock());

    // Release m1 → m2 can now take it.
    m1.release_lock();
    assert!(!m1.holds_lock());
    assert!(m2.acquire_lock(), "lock is free after m1 released");
    assert!(m2.holds_lock());
}

#[test]
fn ap_flag_transitions_write_read_clear() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(dir.path(), Arc::new(ScriptedRunner::new()));
    // Absent → false.
    assert!(!m.read_ap_flag());
    // Write "1\n" → true, exact bytes.
    m.write_ap_flag(true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ap-was-enabled")).unwrap(),
        "1\n"
    );
    assert!(m.read_ap_flag());
    // Write "0\n" → false.
    m.write_ap_flag(false);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ap-was-enabled")).unwrap(),
        "0\n"
    );
    assert!(!m.read_ap_flag());
    // Clear unlinks.
    m.write_ap_flag(true);
    m.clear_ap_flag();
    assert!(!dir.path().join("ap-was-enabled").exists());
}

#[test]
fn client_config_save_is_byte_exact_indent2_plus_newline() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(dir.path(), Arc::new(ScriptedRunner::new()));
    m.save_client_config(&ClientConfig {
        enabled_on_boot: false,
        last_ssid: Some("MyAP".to_string()),
    });
    let body = std::fs::read_to_string(dir.path().join("ground-station-wifi-client.json")).unwrap();
    assert_eq!(
        body,
        "{\n  \"enabled_on_boot\": false,\n  \"last_ssid\": \"MyAP\"\n}\n"
    );
    // No torn tmp.
    assert!(!dir
        .path()
        .join("ground-station-wifi-client.json.tmp")
        .exists());
    // Round-trips.
    let loaded = m.load_client_config();
    assert_eq!(loaded.last_ssid.as_deref(), Some("MyAP"));
}

#[tokio::test]
async fn join_refuses_when_ap_active_and_not_forced() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    }); // is-active → active
    let mut m = mgr(dir.path(), runner);
    let res = m.join("SomeAP", Some("pw"), false).await;
    assert_eq!(res["joined"], false);
    assert_eq!(res["error"], "station_busy_ap_active");
    // Did NOT take the lock on the refusal path.
    assert!(!m.holds_lock());
}

#[tokio::test]
async fn failed_join_releases_lock_and_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive (rc!=0, stdout != "active")
    runner.push(CmdOut::failed(1, "no network with SSID")); // nmcli connect fails
    let mut m = mgr(dir.path(), runner);
    let res = m.join("BadAP", Some("pw"), false).await;
    assert_eq!(res["joined"], false);
    assert_eq!(res["error"], "no network with SSID");
    // Lock released after the failure.
    assert!(!m.holds_lock());
    // last_ssid was never persisted (config file absent).
    assert!(!dir.path().join("ground-station-wifi-client.json").exists());
}

#[tokio::test]
async fn successful_join_keeps_lock_until_leave() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive
    runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
    runner.push(CmdOut {
        rc: 0,
        stdout: "MyAP:wlan0\n".to_string(),
        stderr: String::new(),
    }); // connection show --active → active conn name on wlan0
    runner.push(CmdOut::failed(0, "")); // powersave nmcli
    runner.push(CmdOut::failed(0, "")); // powersave iw
                                        // status(): wifi list + ip addr + ip route.
    runner.push(CmdOut {
        rc: 0,
        stdout: "yes:MyAP:AA\\:BB\\:CC:70:WPA2\n".to_string(),
        stderr: String::new(),
    });
    runner.push(CmdOut {
        rc: 0,
        stdout: "    inet 192.168.5.20/24 scope global wlan0\n".to_string(),
        stderr: String::new(),
    });
    runner.push(CmdOut {
        rc: 0,
        stdout: "default via 192.168.5.1 dev wlan0\n".to_string(),
        stderr: String::new(),
    });
    let m_runner = Arc::clone(&runner);
    let mut m = mgr(dir.path(), runner);
    let res = m.join("MyAP", Some("secret"), false).await;
    assert_eq!(res["joined"], true);
    assert_eq!(res["ip"], "192.168.5.20");
    // Lock is still held after a successful join.
    assert!(m.holds_lock());
    // last_ssid persisted.
    let cfg = m.load_client_config();
    assert_eq!(cfg.last_ssid.as_deref(), Some("MyAP"));

    // The passphrase is NEVER in any recorded argv (it would otherwise be
    // readable in /proc/<pid>/cmdline). It travels on stdin instead.
    let calls = m_runner.recorded();
    assert!(
        !calls.iter().any(|c| c.iter().any(|a| a == "secret")),
        "passphrase leaked into argv: {calls:?}"
    );
    // The connect call used --ask (interactive-secret mode) and did NOT
    // carry a `password` argument.
    let connect = calls
        .iter()
        .find(|c| c.iter().any(|a| a == "connect"))
        .expect("a connect call was recorded");
    assert!(connect.iter().any(|a| a == "--ask"));
    assert!(!connect.iter().any(|a| a == "password"));
    // The secret WAS fed on stdin for that call (trailing newline included).
    let stdins = m_runner.recorded_stdins();
    assert!(
        stdins.iter().any(|s| s == b"secret\n"),
        "passphrase was not supplied on stdin"
    );
}

#[tokio::test]
async fn open_network_join_sends_no_stdin_and_no_password_arg() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive
    runner.push(CmdOut::failed(0, "")); // nmcli connect ok (open, no secret)
    runner.push(CmdOut {
        rc: 0,
        stdout: "OpenNet:wlan0\n".to_string(),
        stderr: String::new(),
    }); // connection show --active
    runner.push(CmdOut::failed(0, "")); // powersave nmcli
    runner.push(CmdOut::failed(0, "")); // powersave iw
    runner.push(CmdOut {
        rc: 0,
        stdout: "yes:OpenNet:AA\\:BB\\:CC:70:\n".to_string(),
        stderr: String::new(),
    }); // wifi list
    runner.push(CmdOut {
        rc: 0,
        stdout: "    inet 192.168.9.5/24 scope global wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip addr
    runner.push(CmdOut {
        rc: 0,
        stdout: "default via 192.168.9.1 dev wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip route
    let m_runner = Arc::clone(&runner);
    let mut m = mgr(dir.path(), runner);
    let res = m.join("OpenNet", None, false).await;
    assert_eq!(res["joined"], true);
    // No `--ask`, no `password`, and no stdin payload for an open network.
    let connect = m_runner
        .recorded()
        .into_iter()
        .find(|c| c.iter().any(|a| a == "connect"))
        .expect("a connect call was recorded");
    assert!(!connect.iter().any(|a| a == "--ask"));
    assert!(!connect.iter().any(|a| a == "password"));
    assert!(
        m_runner.recorded_stdins().iter().all(|s| s.is_empty()),
        "open-network join must not feed any stdin"
    );
}

#[tokio::test]
async fn powersave_runs_both_toggles_after_a_successful_connect() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive
    runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
    runner.push(CmdOut {
        rc: 0,
        stdout: "HomeWifi:wlan0\n".to_string(),
        stderr: String::new(),
    }); // connection show --active
    runner.push(CmdOut::failed(0, "")); // powersave nmcli modify
    runner.push(CmdOut::failed(0, "")); // powersave iw
    runner.push(CmdOut {
        rc: 0,
        stdout: "yes:HomeWifi:AA\\:BB\\:CC:70:WPA2\n".to_string(),
        stderr: String::new(),
    }); // wifi list
    runner.push(CmdOut {
        rc: 0,
        stdout: "    inet 192.168.1.50/24 scope global wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip addr
    runner.push(CmdOut {
        rc: 0,
        stdout: "default via 192.168.1.1 dev wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip route
    let m_runner = Arc::clone(&runner);
    let mut m = mgr(dir.path(), runner);
    let res = m.join("HomeWifi", Some("secret"), false).await;
    assert_eq!(res["joined"], true);

    let calls = m_runner.recorded();
    // Both the connection-level (nmcli) and the runtime (iw) power-save
    // toggles fired.
    let nm_ps = vec![
        "nmcli".to_string(),
        "connection".to_string(),
        "modify".to_string(),
        "HomeWifi".to_string(),
        "802-11-wireless.powersave".to_string(),
        "2".to_string(),
    ];
    let iw_ps = vec![
        "iw".to_string(),
        "dev".to_string(),
        "wlan0".to_string(),
        "set".to_string(),
        "power_save".to_string(),
        "off".to_string(),
    ];
    assert!(calls.contains(&nm_ps), "nmcli powersave toggle missing");
    assert!(calls.contains(&iw_ps), "iw powersave toggle missing");

    // Both toggles come AFTER the connect (the radio must be associated
    // before NM has a connection to modify).
    let connect_idx = calls
        .iter()
        .position(|c| c.iter().any(|a| a == "connect"))
        .expect("a connect call");
    let iw_idx = calls.iter().position(|c| *c == iw_ps).expect("iw toggle");
    let nm_idx = calls.iter().position(|c| *c == nm_ps).expect("nm toggle");
    assert!(iw_idx > connect_idx, "iw toggle ran before connect");
    assert!(nm_idx > connect_idx, "nmcli toggle ran before connect");
}

#[tokio::test]
async fn powersave_toggle_failure_is_nonfatal_to_the_join() {
    // A power-save toggle that fails (driver does not support it) must not
    // fail an otherwise-good join.
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive
    runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
    runner.push(CmdOut {
        rc: 0,
        stdout: "HomeWifi:wlan0\n".to_string(),
        stderr: String::new(),
    }); // connection show --active
    runner.push(CmdOut::failed(1, "not supported")); // powersave nmcli FAILS
    runner.push(CmdOut::failed(1, "not supported")); // powersave iw FAILS
    runner.push(CmdOut {
        rc: 0,
        stdout: "yes:HomeWifi:AA\\:BB\\:CC:70:WPA2\n".to_string(),
        stderr: String::new(),
    }); // wifi list
    runner.push(CmdOut {
        rc: 0,
        stdout: "    inet 192.168.1.50/24 scope global wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip addr
    runner.push(CmdOut {
        rc: 0,
        stdout: "default via 192.168.1.1 dev wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip route
    let mut m = mgr(dir.path(), runner);
    let res = m.join("HomeWifi", Some("secret"), false).await;
    // The join still succeeds and the lock is held.
    assert_eq!(res["joined"], true);
    assert_eq!(res["ip"], "192.168.1.50");
    assert!(m.holds_lock());
}

#[tokio::test]
async fn failed_connect_skips_powersave_entirely() {
    // When the connect itself fails, no power-save toggle is attempted (the
    // failure path restores any AP and releases the lock before returning).
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive (no AP to restore)
    runner.push(CmdOut::failed(1, "No network with SSID 'HomeWifi' found")); // connect FAILS
    let m_runner = Arc::clone(&runner);
    let mut m = mgr(dir.path(), runner);
    let res = m.join("HomeWifi", Some("secret"), false).await;
    assert_eq!(res["joined"], false);
    // Neither power-save toggle was issued.
    let calls = m_runner.recorded();
    assert!(
        !calls
            .iter()
            .any(|c| c.iter().any(|a| a == "802-11-wireless.powersave")),
        "nmcli powersave was issued after a failed connect"
    );
    assert!(
        !calls.iter().any(|c| c.iter().any(|a| a == "power_save")),
        "iw powersave was issued after a failed connect"
    );
    // And no status() probe ran either (no wifi list call recorded).
    assert!(
        !calls.iter().any(|c| c.iter().any(|a| a == "list")),
        "status() ran after a failed connect"
    );
}

#[tokio::test]
async fn powersave_targets_resolved_active_connection_not_ssid() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 3,
        stdout: String::new(),
        stderr: String::new(),
    }); // is-active → inactive
    runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
    runner.push(CmdOut {
        rc: 0,
        // NM auto-named the connection differently from the SSID.
        stdout: "MyAP 1:wlan0\nWired connection 1:eth0\n".to_string(),
        stderr: String::new(),
    }); // connection show --active
    runner.push(CmdOut::failed(0, "")); // powersave nmcli
    runner.push(CmdOut::failed(0, "")); // powersave iw
    runner.push(CmdOut {
        rc: 0,
        stdout: "yes:MyAP:AA\\:BB\\:CC:70:WPA2\n".to_string(),
        stderr: String::new(),
    }); // wifi list
    runner.push(CmdOut {
        rc: 0,
        stdout: "    inet 192.168.5.20/24 scope global wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip addr
    runner.push(CmdOut {
        rc: 0,
        stdout: "default via 192.168.5.1 dev wlan0\n".to_string(),
        stderr: String::new(),
    }); // ip route
    let m_runner = Arc::clone(&runner);
    let mut m = mgr(dir.path(), runner);
    let res = m.join("MyAP", Some("secret"), false).await;
    assert_eq!(res["joined"], true);
    // The `connection modify` call targets the resolved active name
    // ("MyAP 1"), not the SSID ("MyAP").
    let modify = m_runner
        .recorded()
        .into_iter()
        .find(|c| c.iter().any(|a| a == "modify"))
        .expect("a connection modify call was recorded");
    assert!(
        modify.iter().any(|a| a == "MyAP 1"),
        "powersave modify did not target the resolved connection name: {modify:?}"
    );
    assert!(!modify.iter().any(|a| a == "MyAP"));
}

#[tokio::test]
async fn set_autoconnect_rejects_an_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(dir.path(), Arc::new(ScriptedRunner::new()));
    let res = m.set_autoconnect("", true).await;
    assert_eq!(
        res,
        json!({"autoconnect": false, "name": "", "error": "name_required"})
    );
}

#[tokio::test]
async fn set_autoconnect_enables_via_nmcli_modify_yes() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut::failed(0, "")); // nmcli modify ok
    let m_runner = Arc::clone(&runner);
    let m = mgr(dir.path(), runner);
    let res = m.set_autoconnect("HomeNet", true).await;
    assert_eq!(
        res,
        json!({"autoconnect": true, "name": "HomeNet", "error": null})
    );
    // The exact nmcli args: modify <name> connection.autoconnect yes.
    let call = &m_runner.recorded()[0];
    assert_eq!(
        call,
        &vec![
            "nmcli",
            "connection",
            "modify",
            "HomeNet",
            "connection.autoconnect",
            "yes",
        ]
    );
}

#[tokio::test]
async fn set_autoconnect_disables_via_nmcli_modify_no() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut::failed(0, "")); // nmcli modify ok
    let m_runner = Arc::clone(&runner);
    let m = mgr(dir.path(), runner);
    let res = m.set_autoconnect("HomeNet", false).await;
    assert_eq!(
        res,
        json!({"autoconnect": false, "name": "HomeNet", "error": null})
    );
    let call = &m_runner.recorded()[0];
    assert_eq!(call.last().map(String::as_str), Some("no"));
}

#[tokio::test]
async fn set_autoconnect_surfaces_the_nmcli_error() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut::failed(10, "unknown connection 'Nope'")); // nmcli fails
    let m = mgr(dir.path(), runner);
    let res = m.set_autoconnect("Nope", true).await;
    // The intended autoconnect value is still echoed; the error is the
    // trimmed stderr.
    assert_eq!(res["autoconnect"], json!(true));
    assert_eq!(res["name"], json!("Nope"));
    assert_eq!(res["error"], json!("unknown connection 'Nope'"));
}

#[tokio::test]
async fn set_autoconnect_falls_back_to_nmcli_failed_on_empty_streams() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut::failed(1, "")); // nonzero, no stderr/stdout
    let m = mgr(dir.path(), runner);
    let res = m.set_autoconnect("Net", false).await;
    assert_eq!(res["error"], json!("nmcli_failed"));
}
