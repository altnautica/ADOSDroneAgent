//! Mesh bring-up decision tests: the wpa_supplicant config, the gateway mode,
//! the gateway pin, and the mesh interface choice.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::bringup::DIRECT_ROLE_REQUEST;
use super::gateway::{gateway_mode_for, gw_mode_args, pinned_gateway};
use super::iface::{
    driver_mesh_iface, pick_mesh_iface, published_mesh_adapter, ROLE_SIDECAR_MAX_AGE,
};
use super::secure_join::{
    bat_member, channel_freq_mhz, mesh_passphrase, parse_iw_info, wpa_supplicant_conf,
};

const PSK: [u8; 32] = [
    0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

fn lines(conf: &str) -> Vec<String> {
    conf.lines().map(|l| l.trim().to_string()).collect()
}

#[test]
fn mesh_80211s_config_uses_sae_keyed_from_the_psk() {
    let phrase = mesh_passphrase(&PSK);
    let conf = wpa_supplicant_conf("802.11s", "ados-abc", &phrase, 2412).unwrap();
    let l = lines(&conf);
    for expected in [
        "ssid=\"ados-abc\"".to_string(),
        "mode=5".into(),
        "frequency=2412".into(),
        "key_mgmt=SAE".into(),
        format!("sae_password=\"{phrase}\""),
        "ieee80211w=2".into(),
    ] {
        assert!(l.contains(&expected), "{expected}");
    }
}

#[test]
fn ibss_config_uses_rsn_ccmp_keyed_from_the_psk() {
    let phrase = mesh_passphrase(&PSK);
    let conf = wpa_supplicant_conf("ibss", "ados-abc", &phrase, 2437).unwrap();
    let l = lines(&conf);
    assert_eq!(l[0], "ap_scan=2");
    for expected in [
        "mode=1".to_string(),
        "frequency=2437".into(),
        "proto=RSN".into(),
        "key_mgmt=WPA-PSK".into(),
        "pairwise=CCMP".into(),
        "group=CCMP".into(),
        format!("psk=\"{phrase}\""),
    ] {
        assert!(l.contains(&expected), "{expected}");
    }
}

#[test]
fn passphrase_is_a_valid_wpa_passphrase_shared_by_every_holder() {
    let phrase = mesh_passphrase(&PSK);
    assert!((8..=63).contains(&phrase.len()));
    let copy = PSK;
    assert_eq!(phrase, mesh_passphrase(&copy));
    let mut reversed = PSK;
    reversed.reverse();
    assert_ne!(phrase, mesh_passphrase(&reversed));
}

#[test]
fn unsafe_values_are_refused() {
    let phrase = mesh_passphrase(&PSK);
    let long = "x".repeat(33);
    for mesh_id in [
        "ados\"\n}\nnetwork={",
        "ados\nkey_mgmt=NONE",
        "",
        long.as_str(),
        " ados",
        "ados\\",
    ] {
        assert!(
            wpa_supplicant_conf("802.11s", mesh_id, &phrase, 2412).is_err(),
            "{mesh_id:?}"
        );
    }
    assert!(wpa_supplicant_conf("ibss", "ados-abc", "short", 2412).is_err());
    assert!(wpa_supplicant_conf("wds", "ados-abc", &phrase, 2412).is_err());
}

#[test]
fn channel_freq_maps_only_the_2g4_channels() {
    assert_eq!(channel_freq_mhz(1), Ok(2412));
    assert_eq!(channel_freq_mhz(6), Ok(2437));
    assert_eq!(channel_freq_mhz(13), Ok(2472));
    assert!(channel_freq_mhz(0).is_err());
    assert!(channel_freq_mhz(36).is_err());
}

#[test]
fn iw_info_parse_reads_type_and_ssid() {
    let out = "Interface wlan1\n\tifindex 5\n\ttype mesh point\n\tssid ados-abc\n\twiphy 1\n";
    assert_eq!(
        parse_iw_info(out),
        (Some("mesh point".to_string()), Some("ados-abc".to_string()))
    );
    assert_eq!(parse_iw_info("Interface wlan1\n"), (None, None));
}

#[test]
fn gateway_mode_follows_role_policy_and_uplink() {
    assert_eq!(gateway_mode_for("receiver", "auto", true), "server");
    assert_eq!(gateway_mode_for("receiver", "auto", false), "client");
    assert_eq!(gateway_mode_for("relay", "auto", false), "off");
    assert_eq!(gateway_mode_for("relay", "force_on", false), "server");
    assert_eq!(gateway_mode_for("receiver", "force_off", true), "client");
    assert_eq!(gw_mode_args("server"), ["gw_mode", "server", "10000/2000"]);
    assert_eq!(gw_mode_args("client"), ["gw_mode", "client"]);
}

#[test]
fn only_a_pinned_preference_names_a_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("gateway.json");
    assert_eq!(pinned_gateway(&p), None);
    std::fs::write(
        &p,
        r#"{"mode": "pinned", "pinned_mac": "aa:bb:cc:dd:ee:01"}"#,
    )
    .unwrap();
    assert_eq!(pinned_gateway(&p).as_deref(), Some("aa:bb:cc:dd:ee:01"));
    std::fs::write(&p, r#"{"mode": "auto", "pinned_mac": "aa:bb:cc:dd:ee:01"}"#).unwrap();
    assert_eq!(pinned_gateway(&p), None);
    std::fs::write(&p, r#"{"mode": "pinned", "pinned_mac": null}"#).unwrap();
    assert_eq!(pinned_gateway(&p), None);
    std::fs::write(&p, "{not json").unwrap();
    assert_eq!(pinned_gateway(&p), None);
}

fn write_sidecar(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

#[test]
fn a_fresh_role_sidecar_names_the_mesh_adapter() {
    let dir = tempfile::tempdir().unwrap();
    write_sidecar(
        dir.path(),
        "wfb-relay.json",
        r#"{"role":"relay","mesh_adapter":"wlan3"}"#,
    );
    write_sidecar(
        dir.path(),
        "wfb-receiver.json",
        r#"{"role":"receiver","mesh_adapter":"wlan4"}"#,
    );
    let now = SystemTime::now();
    assert_eq!(
        published_mesh_adapter("relay", dir.path(), now).as_deref(),
        Some("wlan3")
    );
    assert_eq!(
        published_mesh_adapter("receiver", dir.path(), now).as_deref(),
        Some("wlan4")
    );
    assert_eq!(published_mesh_adapter("direct", dir.path(), now), None);
}

#[test]
fn no_usable_publication_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    assert_eq!(published_mesh_adapter("relay", dir.path(), now), None);
    write_sidecar(
        dir.path(),
        "wfb-relay.json",
        r#"{"role":"relay","mesh_adapter":null}"#,
    );
    assert_eq!(published_mesh_adapter("relay", dir.path(), now), None);
    write_sidecar(dir.path(), "wfb-relay.json", r#"{"role":"relay"}"#);
    assert_eq!(published_mesh_adapter("relay", dir.path(), now), None);
    write_sidecar(dir.path(), "wfb-relay.json", r#"{"mesh_adapter":"wlan3"}"#);
    let later = now + ROLE_SIDECAR_MAX_AGE + Duration::from_secs(5);
    assert_eq!(published_mesh_adapter("relay", dir.path(), later), None);
    write_sidecar(dir.path(), "wfb-relay.json", "{not json");
    assert_eq!(published_mesh_adapter("relay", dir.path(), now), None);
}

/// A fake `/sys/class/net` wireless netdev bound to `driver`.
fn fake_netdev(sys: &Path, iface: &str, driver: &str) {
    let dev = sys.join(iface);
    std::fs::create_dir_all(dev.join("wireless")).unwrap();
    let drv = sys.join("drivers").join(driver);
    std::fs::create_dir_all(&drv).unwrap();
    std::fs::create_dir_all(dev.join("device")).unwrap();
    std::os::unix::fs::symlink(&drv, dev.join("device/driver")).unwrap();
}

#[test]
fn pick_prefers_override_then_published_then_driver_roles() {
    let sys = tempfile::tempdir().unwrap();
    let run = tempfile::tempdir().unwrap();
    fake_netdev(sys.path(), "wlan0", "brcmfmac");
    fake_netdev(sys.path(), "wlan1", "rtl88x2eu");
    fake_netdev(sys.path(), "wlan2", "mt7601u");
    // The WFB radio is never the mesh; the second non-WFB radio is.
    assert_eq!(driver_mesh_iface(sys.path()).as_deref(), Some("wlan2"));
    assert_eq!(
        pick_mesh_iface(None, "relay", run.path(), sys.path()).as_deref(),
        Some("wlan2")
    );
    write_sidecar(run.path(), "wfb-relay.json", r#"{"mesh_adapter":"wlan5"}"#);
    assert_eq!(
        pick_mesh_iface(None, "relay", run.path(), sys.path()).as_deref(),
        Some("wlan5")
    );
    assert_eq!(
        pick_mesh_iface(Some("wlan7"), "relay", run.path(), sys.path()).as_deref(),
        Some("wlan7")
    );
}

#[test]
fn a_single_control_radio_carries_the_mesh() {
    let sys = tempfile::tempdir().unwrap();
    fake_netdev(sys.path(), "wlan0", "aic8800_fdrv");
    assert_eq!(driver_mesh_iface(sys.path()).as_deref(), Some("wlan0"));
}

#[test]
fn bat_membership_reads_the_sysfs_link() {
    let sys = tempfile::tempdir().unwrap();
    let dir = sys.path().join("wlan1/batman_adv");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mesh_iface"), "bat0\n").unwrap();
    assert!(bat_member(sys.path(), "wlan1", "bat0"));
    assert!(!bat_member(sys.path(), "wlan1", "bat1"));
    assert!(!bat_member(sys.path(), "wlan9", "bat0"));
}

#[tokio::test]
async fn missing_identity_asks_the_supervisor_for_direct() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("supervisor.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        write.write_all(b"{\"ok\": true}\n").await.unwrap();
        line
    });
    super::bringup::request_direct_role(&sock).await;
    let line = server.await.unwrap();
    assert_eq!(line, format!("{DIRECT_ROLE_REQUEST}\n"));
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        request,
        serde_json::json!({"op": "set_role", "role": "direct", "reason": "mesh_identity_missing"})
    );
}
