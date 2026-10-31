use super::*;
use crate::cmd::testing::ScriptedRunner;
use crate::cmd::CmdOut;
use std::os::unix::fs::PermissionsExt;

fn mgr(dir: &std::path::Path, device_id: &str, runner: Arc<ScriptedRunner>) -> HostapdManager {
    let mut m = HostapdManager::with_paths(
        device_id,
        None,
        6,
        String::new(),
        runner,
        dir.join("hostapd-gs.conf"),
        dir.join("dnsmasq-gs.conf"),
        dir.join("ap-passphrase"),
    );
    m.interface = "wlan0".to_string();
    m
}

#[test]
fn short_id_takes_four_hex_uppercased_and_pads() {
    // 'a' and 'd' from "ados" ARE hex, so the stripped string is
    // "ad58c27faf" → first four "ad58" → "AD58" (matches the Python regex).
    assert_eq!(short_id("ados-58c27faf"), "AD58");
    assert_eq!(build_ssid("ados-58c27faf"), "ADOS-GS-AD58");
    // A pure-hex id is taken verbatim.
    assert_eq!(short_id("58c27faf"), "58C2");
    // Short id pads with zeros.
    assert_eq!(short_id("ab"), "AB00");
    // Empty → all zeros.
    assert_eq!(short_id(""), "0000");
    // 'g' is not hex; only a/b/c/d/e/f + digits count.
    assert_eq!(short_id("ggggde12"), "DE12");
}

#[test]
fn the_ap_country_follows_the_operators_pinned_region() {
    // It was a hardcoded IN while the radio's own reconciler defaulted to
    // US, so a stock box declared two different jurisdictions at once.
    assert_eq!(ados_protocol::ap_country::from_yaml(""), "US");
    assert_eq!(
        ados_protocol::ap_country::from_yaml(
            "network:\n  regulatory:\n    mode: region\n    region: IN\n"
        ),
        "IN",
        "an operator who pins a region still gets it"
    );
}

#[test]
fn concurrent_first_boot_generation_converges_on_one_value() {
    // Several processes resolve this on a fresh boot — the native AP
    // manager, the Python one, and (before it was made read-only) a status
    // route. Each drew its own value and each wrote the file, so the value
    // an operator was shown could differ from the one hostapd loaded and
    // the access point was unjoinable. Ordering them is fragile; an
    // exclusive create is not.
    let dir = tempfile::tempdir().unwrap();
    let mut first = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    let mut second = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    let mut third = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));

    let a = first.ensure_passphrase();
    let b = second.ensure_passphrase();
    let c = third.ensure_passphrase();

    assert_eq!(a, b, "a second resolver must adopt the first's value");
    assert_eq!(b, c, "and so must a third");
    let on_disk = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
    assert_eq!(
        on_disk.trim(),
        a,
        "the displayed value must be the one on disk"
    );
}

#[test]
fn a_configured_passphrase_wins_over_generating() {
    // The doc promised this and the path that runs made it unreachable: a
    // fleet setting one shared credential got a random value per box.
    let dir = tempfile::tempdir().unwrap();
    let mut m = HostapdManager::with_paths(
        "dead",
        None,
        6,
        "FleetShared2026".to_string(),
        Arc::new(ScriptedRunner::new()),
        dir.path().join("hostapd-gs.conf"),
        dir.path().join("dnsmasq-gs.conf"),
        dir.path().join("ap-passphrase"),
    );
    assert_eq!(m.ensure_passphrase(), "FleetShared2026");
}

#[test]
fn a_generated_passphrase_is_stable_across_restarts() {
    // A generated value that is not written is a DIFFERENT passphrase on
    // every restart: the operator reads one off the installer card, the
    // service restarts, and the network they were told to join is gone.
    let dir = tempfile::tempdir().unwrap();
    let mut first = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    let generated = first.ensure_passphrase();

    // Same box, fresh manager — as after a service restart.
    let mut again = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    assert_eq!(
        again.ensure_passphrase(),
        generated,
        "the generated passphrase must survive a restart"
    );

    let on_disk = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
    assert_eq!(on_disk, format!("{generated}\n"));
}

#[test]
fn a_generated_passphrase_is_persisted_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut m = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    m.ensure_passphrase();
    let mode = std::fs::metadata(dir.path().join("ap-passphrase"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the passphrase file must not be world-readable"
    );
}

#[test]
fn ensure_passphrase_precedence_file_then_config_then_generated() {
    let dir = tempfile::tempdir().unwrap();
    // No file, no config → a fresh per-unit value, NOT the shared builtin.
    // One published default across every shipped unit is not a secret:
    // anyone in radio range of any ground station could join any other.
    let mut m = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    let generated = m.ensure_passphrase();
    assert_ne!(
        generated, "altnautica",
        "a fresh unit must not come up on the shared built-in passphrase"
    );
    assert!(
        ados_protocol::secret_gen::is_valid_wpa2_passphrase(&generated),
        "hostapd refuses the whole config on an illegal passphrase"
    );

    // And it is per-unit: a second box does not get the same one.
    let dir2 = tempfile::tempdir().unwrap();
    let mut other = mgr(dir2.path(), "beef", Arc::new(ScriptedRunner::new()));
    assert_ne!(
        other.ensure_passphrase(),
        generated,
        "two units must not share a generated passphrase"
    );

    // Configured password wins over builtin (no file present).
    let mut m2 = HostapdManager::with_paths(
        "dead",
        None,
        6,
        "configured-pw".to_string(),
        Arc::new(ScriptedRunner::new()),
        dir.path().join("h2.conf"),
        dir.path().join("d2.conf"),
        dir.path().join("ap-passphrase-2"),
    );
    assert_eq!(m2.ensure_passphrase(), "configured-pw");

    // Existing file wins over everything.
    std::fs::write(dir.path().join("ap-passphrase"), "from-file\n").unwrap();
    let mut m3 = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
    assert_eq!(m3.ensure_passphrase(), "from-file");
}

#[test]
fn a_written_conf_always_carries_a_wpa2_legal_passphrase() {
    // The invariant behind the fail-closed guard in `write_config`: no
    // hostapd.conf is ever emitted with an empty or illegal passphrase,
    // which would either open the network or make hostapd fail at start
    // for reasons that read as an unrelated fault.
    //
    // Honest limit: this exercises the success path. The RNG-failure branch
    // that leaves the passphrase empty is not reachable from a test without
    // injecting a `getrandom` failure, so the guard itself is defence in
    // depth rather than something proven here.
    let dir = tempfile::tempdir().unwrap();
    let mut m = mgr(dir.path(), "c0ffee", Arc::new(ScriptedRunner::new()));
    m.country_code = "US".to_string();
    m.write_config().unwrap();

    let body = std::fs::read_to_string(dir.path().join("hostapd-gs.conf")).unwrap();
    let line = body
        .lines()
        .find(|l| l.starts_with("wpa_passphrase="))
        .expect("a written conf must set wpa_passphrase");
    let value = line.trim_start_matches("wpa_passphrase=");
    assert!(
        !value.is_empty(),
        "an empty passphrase must never be written"
    );
    assert!(
        ados_protocol::secret_gen::is_valid_wpa2_passphrase(value),
        "written passphrase must satisfy WPA2's 8-63 character rule"
    );
}

#[test]
fn hostapd_conf_is_byte_exact_with_0600_mode() {
    let dir = tempfile::tempdir().unwrap();
    // Pin the passphrase through config so the golden body stays exact:
    // an unconfigured unit now generates a fresh one per box.
    let mut m = HostapdManager::with_paths(
        "58c27faf",
        None,
        6,
        "altnautica".to_string(),
        Arc::new(ScriptedRunner::new()),
        dir.path().join("hostapd-gs.conf"),
        dir.path().join("dnsmasq-gs.conf"),
        dir.path().join("ap-passphrase"),
    );
    m.interface = "wlan0".to_string();
    m.ensure_passphrase(); // → the configured "altnautica"
                           // Pin the country so the golden body does not depend on the host's
                           // /etc/ados/config.yaml. An unpinned unit resolves to the same default.
    m.country_code = "US".to_string();
    m.write_config().unwrap();

    let expected = "# ADOS Ground Station hostapd config for ADOS-GS-58C2\n\
interface=wlan0\n\
driver=nl80211\n\
ssid=ADOS-GS-58C2\n\
hw_mode=g\n\
channel=6\n\
country_code=US\n\
ieee80211n=1\n\
ieee80211d=1\n\
wmm_enabled=1\n\
auth_algs=1\n\
macaddr_acl=0\n\
ignore_broadcast_ssid=0\n\
wpa=2\n\
wpa_passphrase=altnautica\n\
wpa_key_mgmt=WPA-PSK\n\
wpa_pairwise=CCMP\n\
rsn_pairwise=CCMP\n";
    let body = std::fs::read_to_string(dir.path().join("hostapd-gs.conf")).unwrap();
    assert_eq!(body, expected);
    let mode = std::fs::metadata(dir.path().join("hostapd-gs.conf"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn dnsmasq_conf_is_byte_exact_with_0644_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = mgr(dir.path(), "58c27faf", Arc::new(ScriptedRunner::new()));
    m.ensure_passphrase();
    m.write_config().unwrap();

    let expected = "# ADOS Ground Station DHCP for wlan0\n\
interface=wlan0\n\
bind-interfaces\n\
except-interface=lo\n\
dhcp-range=192.168.4.10,192.168.4.100,12h\n\
dhcp-option=3,192.168.4.1\n\
dhcp-option=6,192.168.4.1\n\
domain-needed\n\
bogus-priv\n\
no-resolv\n";
    let body = std::fs::read_to_string(dir.path().join("dnsmasq-gs.conf")).unwrap();
    assert_eq!(body, expected);
    let mode = std::fs::metadata(dir.path().join("dnsmasq-gs.conf"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
}

#[tokio::test]
async fn apply_ap_config_writes_passphrase_0600_with_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    // write_config writes the two confs; restart is one systemctl call.
    let mut m = mgr(dir.path(), "58c27faf", runner.clone());
    m.ensure_passphrase();
    let ok = m.apply_ap_config(None, Some("new-secret"), None).await;
    assert!(ok);
    let pw = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
    assert_eq!(pw, "new-secret\n");
    let mode = std::fs::metadata(dir.path().join("ap-passphrase"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    // It issued a hostapd restart.
    assert!(runner
        .recorded()
        .iter()
        .any(|c| c.contains(&"restart".to_string())));
}

#[tokio::test]
async fn apply_ap_config_reports_a_failed_hostapd_restart() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    let mut m = mgr(dir.path(), "58c27faf", runner.clone());
    m.ensure_passphrase();
    // The restart is the first command the apply runs.
    runner.push(CmdOut::failed(1, "Job for hostapd.service failed"));
    assert!(!m.apply_ap_config(None, Some("new-secret"), None).await);
    assert_eq!(runner.recorded().len(), 1);
}

#[tokio::test]
async fn apply_ap_config_refuses_settings_that_would_inject_directives() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    let mut m = mgr(dir.path(), "58c27faf", runner.clone());
    m.ensure_passphrase();
    let before = m.passphrase().to_string();
    // A passphrase that would land `wpa=0` after `wpa=2`.
    assert!(!m.apply_ap_config(None, Some("x\nwpa=0"), None).await);
    assert!(
        !m.apply_ap_config(Some("ap\nctrl_interface=/tmp"), None, None)
            .await
    );
    assert!(!m.apply_ap_config(None, Some("short"), None).await);
    assert!(!m.apply_ap_config(None, None, Some(36)).await);
    assert_eq!(m.passphrase(), before);
    assert!(!dir.path().join("hostapd-gs.conf").exists());
    assert!(runner.recorded().is_empty(), "nothing restarted");
}

#[test]
fn write_config_refuses_an_invalid_loaded_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = mgr(dir.path(), "58c27faf", Arc::new(ScriptedRunner::new()));
    m.passphrase = "open\nwpa=0".to_string();
    assert!(m.write_config().is_err());
    assert!(!dir.path().join("hostapd-gs.conf").exists());
}

#[test]
fn write_config_refuses_an_unresolved_interface() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = mgr(dir.path(), "58c27faf", Arc::new(ScriptedRunner::new()));
    m.interface.clear();
    m.ensure_passphrase();
    assert!(m.write_config().is_err());
}

/// `iw dev <iface> info` for a ground station whose AP is up.
fn iw_info_ap(channel: Option<u32>) -> String {
    let mut s = String::from(
        "Interface wlan0\n\tifindex 3\n\twdev 0x1\n\taddr b8:27:eb:11:22:33\n\
         \tssid ADOS-GS-58C2\n\ttype AP\n\twiphy 0\n",
    );
    if let Some(c) = channel {
        s.push_str(&format!(
            "\tchannel {c} (2437 MHz), width: 20 MHz, center1: 2437 MHz\n"
        ));
    }
    s.push_str("\ttxpower 31.00 dBm\n");
    s
}

/// Seed a fake `/sys/class/net/<iface>/statistics/tx_packets`.
fn seed_tx(root: &std::path::Path, iface: &str, packets: u64) {
    let dir = root.join(iface).join("statistics");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tx_packets"), format!("{packets}\n")).unwrap();
}

#[test]
fn iw_info_parses_mode_channel_and_ssid_and_flags_an_unusable_answer() {
    let up = parse_iw_info(&iw_info_ap(Some(6)));
    assert!(up.probe_ok);
    assert_eq!(up.iface_type.as_deref(), Some("AP"));
    assert_eq!(up.operating_channel, Some(6));
    assert_eq!(up.ssid.as_deref(), Some("ADOS-GS-58C2"));

    // In AP mode but with no channel context: hostapd is alive, the BSS is
    // not. That distinction is the whole point of reading the channel line.
    let idle = parse_iw_info(&iw_info_ap(None));
    assert!(idle.probe_ok);
    assert_eq!(idle.operating_channel, None);

    // No `type` line at all: the probe did not answer. It must NOT read as
    // "answered, and it is not an AP" — nothing may be concluded from it.
    let unusable = parse_iw_info("command failed: No such device (-19)\n");
    assert!(!unusable.probe_ok);
    assert_eq!(unusable.iface_type, None);
}

#[tokio::test]
async fn status_reports_running_from_the_radio_not_from_the_unit() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    }); // systemctl is-active → active
    runner.push(CmdOut {
        rc: 0,
        stdout: iw_info_ap(Some(6)),
        stderr: String::new(),
    }); // iw dev wlan0 info
    runner.push(CmdOut {
        rc: 0,
        stdout: "Station AA:BB:CC:DD:EE:FF (on wlan0)\n\tinactive time:\t10 ms\nStation 11:22:33:44:55:66 (on wlan0)\n".to_string(),
        stderr: String::new(),
    }); // iw dev wlan0 station dump
    let mut m = mgr(dir.path(), "58c27faf", runner);
    m.set_sysfs_net_root(dir.path().to_path_buf());
    seed_tx(dir.path(), "wlan0", 4200);

    let st = m.status().await;
    assert_eq!(st["running"], true);
    assert_eq!(st["hostapd_unit_active"], true);
    assert_eq!(st["radio"]["probe_ok"], true);
    assert_eq!(st["radio"]["iface_type"], "AP");
    assert_eq!(st["radio"]["operating_channel"], 6);
    assert_eq!(st["radio"]["tx_packets"], 4200);
    let clients = st["connected_clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert_eq!(clients[0], "aa:bb:cc:dd:ee:ff");
    assert_eq!(clients[1], "11:22:33:44:55:66");
}

#[tokio::test]
async fn an_active_unit_whose_radio_is_not_an_ap_reports_not_running() {
    // The defect this whole change exists for: the unit is `active` and no
    // SSID is on the air. A status surface that answered `running: true`
    // here is the one that hid a dead AP on every ground station.
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    });
    runner.push(CmdOut {
        rc: 0,
        stdout: "Interface wlan0\n\tifindex 3\n\ttype managed\n".to_string(),
        stderr: String::new(),
    });
    let mut m = mgr(dir.path(), "58c27faf", runner);
    m.set_sysfs_net_root(dir.path().to_path_buf());

    let st = m.status().await;
    assert_eq!(st["running"], false);
    // …while still saying plainly that the daemon itself is up, so the
    // operator can tell "hostapd died" from "hostapd is not serving".
    assert_eq!(st["hostapd_unit_active"], true);
    assert_eq!(st["radio"]["iface_type"], "managed");
    // Nothing was scraped for stations: the interface is not an AP.
    assert_eq!(st["connected_clients"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn supervise_restarts_hostapd_when_the_radio_is_not_serving_the_ap() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    }); // is-active
    runner.push(CmdOut {
        rc: 0,
        stdout: "Interface wlan0\n\ttype managed\n".to_string(),
        stderr: String::new(),
    }); // iw info → not an AP
    let mut m = mgr(dir.path(), "58c27faf", runner.clone());
    m.set_sysfs_net_root(dir.path().to_path_buf());

    assert_eq!(m.supervise().await, ApLiveness::Stalled("not_ap_mode"));
    assert!(
        runner
            .recorded()
            .iter()
            .any(|c| c.contains(&"restart".to_string()) && c.contains(&HOSTAPD_UNIT.to_string())),
        "a wedged AP was not restarted: {:?}",
        runner.recorded()
    );
}

#[tokio::test]
async fn supervise_never_restarts_on_a_probe_it_could_not_make() {
    // No `iw`, or the interface is gone. Acting on that would turn a
    // missing tool into a restart loop of the operator's only reachability.
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: "active\n".to_string(),
        stderr: String::new(),
    }); // is-active
    runner.push(CmdOut::failed(1, "command failed: No such device (-19)")); // iw info
    let mut m = mgr(dir.path(), "58c27faf", runner.clone());
    m.set_sysfs_net_root(dir.path().to_path_buf());

    assert_eq!(
        m.supervise().await,
        ApLiveness::Unknown("radio_unprobeable")
    );
    assert!(
        !runner
            .recorded()
            .iter()
            .any(|c| c.contains(&"restart".to_string())),
        "an unprobeable radio must not trigger a restart: {:?}",
        runner.recorded()
    );
}

#[test]
fn a_flat_transmit_counter_accumulates_and_a_moving_one_resets_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    let m = mgr(dir.path(), "58c27faf", runner);
    let t0 = Instant::now();

    // First reading only establishes the baseline.
    assert_eq!(m.observe_tx(Some(100), t0), Some(Duration::ZERO));
    // Same value later: that elapsed time IS the flat window.
    assert_eq!(
        m.observe_tx(Some(100), t0 + Duration::from_secs(31)),
        Some(Duration::from_secs(31))
    );
    // The counter moves: the window restarts from zero.
    assert_eq!(
        m.observe_tx(Some(101), t0 + Duration::from_secs(32)),
        Some(Duration::ZERO)
    );
    // An unreadable counter is a GAP, not elapsed flatness: it clears the
    // baseline so a transient read failure cannot age into a stall verdict.
    assert_eq!(m.observe_tx(None, t0 + Duration::from_secs(33)), None);
    assert_eq!(
        m.observe_tx(Some(101), t0 + Duration::from_secs(99)),
        Some(Duration::ZERO)
    );
}

#[tokio::test]
async fn a_flat_counter_with_no_station_is_not_a_stall() {
    // mac80211 does not account beacons into the netdev counter, so an idle
    // AP with nobody associated legitimately sits flat forever. Condemning
    // it would restart a healthy AP every window.
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: iw_info_ap(Some(6)),
        stderr: String::new(),
    }); // iw info → AP on channel 6
    runner.push(CmdOut {
        rc: 0,
        stdout: String::new(),
        stderr: String::new(),
    }); // station dump → nobody associated
    let mut m = mgr(dir.path(), "58c27faf", runner);
    m.set_sysfs_net_root(dir.path().to_path_buf());
    seed_tx(dir.path(), "wlan0", 7);
    // Age the baseline well past the stall window.
    *m.tx_sample.lock() = Some(TxSample {
        packets: 7,
        flat_since: Instant::now() - TX_STALL_WINDOW * 4,
    });

    assert_eq!(m.check_liveness().await, ApLiveness::Radiating);
}

#[tokio::test]
async fn a_flat_counter_with_an_associated_station_is_a_stall() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(ScriptedRunner::new());
    runner.push(CmdOut {
        rc: 0,
        stdout: iw_info_ap(Some(6)),
        stderr: String::new(),
    }); // iw info → AP on channel 6
    runner.push(CmdOut {
        rc: 0,
        stdout: "Station aa:bb:cc:dd:ee:ff (on wlan0)\n".to_string(),
        stderr: String::new(),
    }); // station dump → one client
    let mut m = mgr(dir.path(), "58c27faf", runner);
    m.set_sysfs_net_root(dir.path().to_path_buf());
    seed_tx(dir.path(), "wlan0", 7);
    *m.tx_sample.lock() = Some(TxSample {
        packets: 7,
        flat_since: Instant::now() - TX_STALL_WINDOW * 4,
    });

    assert_eq!(
        m.check_liveness().await,
        ApLiveness::Stalled("tx_flat_with_stations")
    );
}
