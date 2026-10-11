use super::*;
use std::io::Write;

fn temp_yaml(name: &str, body: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ados-cloud-cfg-{}-{}.yaml",
        std::process::id(),
        name
    ));
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

#[test]
fn missing_file_yields_defaults() {
    let cfg = CloudConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
    assert_eq!(cfg.server.cloud.url, "https://convex-site.altnautica.com");
}

#[test]
fn reads_cloud_section_ignoring_the_rest() {
    let yaml = "\
mavlink:
  port: /dev/ttyACM0
server:
  mode: cloud
  cloud:
    url: https://relay.example/convex
video:
  mode: disabled
";
    let path = temp_yaml("full", yaml);
    let cfg = CloudConfig::load_from(&path);
    assert_eq!(cfg.server.cloud.url, "https://relay.example/convex");
    // Default arm flag is the schema default (on) when no wfb section is present.
    assert!(cfg.video.wfb.auto_pair_enabled);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cloud_logs_enabled_defaults_off_and_reads_the_opt_in() {
    // Absent → off.
    let cfg = CloudConfig::default();
    assert!(!cfg.cloud_logs_enabled());

    // Present and true → on.
    let yaml = "\
server:
  mode: cloud
  cloud_logs_enabled: true
";
    let path = temp_yaml("cloudlogs", yaml);
    let cfg = CloudConfig::load_from(&path);
    assert!(cfg.cloud_logs_enabled());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reads_the_wfb_auto_pair_arm_flag() {
    let yaml = "\
video:
  wfb:
    channel: 149
    auto_pair_enabled: false
";
    let path = temp_yaml("wfb", yaml);
    let cfg = CloudConfig::load_from(&path);
    assert!(!cfg.video.wfb.auto_pair_enabled);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn effective_convex_url_is_an_allowlist_failing_closed() {
    // The convex URL is loaded from `pairing.convex_url`; the mode decides
    // whether it is surfaced. Build a config body that carries a URL and a
    // chosen mode, then assert the gate per mode.
    let with_mode = |mode: &str| {
        let yaml = format!(
            "\
server:
  mode: {mode}
pairing:
  convex_url: https://relay.example/convex
"
        );
        let path = temp_yaml(&format!("mode-{mode}"), &yaml);
        let cfg = CloudConfig::load_from(&path);
        let url = cfg.effective_convex_url();
        let enabled = cfg.cloud_relay_enabled();
        let _ = std::fs::remove_file(&path);
        (url, enabled)
    };

    // Explicit cloud posture → the URL is surfaced and the relay is on.
    let (url, enabled) = with_mode("cloud");
    assert_eq!(url, "https://relay.example/convex");
    assert!(enabled);

    let (url, enabled) = with_mode("self_hosted");
    assert_eq!(url, "https://relay.example/convex");
    assert!(enabled);

    // Local mode → empty, relay off.
    let (url, enabled) = with_mode("local");
    assert!(url.is_empty());
    assert!(!enabled);

    // An unknown / typo'd mode fails CLOSED — empty, relay off (the
    // allowlist's whole point: a typo never beacons to the cloud).
    let (url, enabled) = with_mode("weird");
    assert!(url.is_empty());
    assert!(!enabled);

    // Absent mode (the all-defaults config defaults the mode to "local")
    // → empty, relay off.
    let cfg = CloudConfig::default();
    assert!(cfg.effective_convex_url().is_empty());
    assert!(!cfg.cloud_relay_enabled());

    // A config with NO server section at all also defaults to off.
    let path = temp_yaml(
        "no-server",
        "pairing:\n  convex_url: https://relay.example/convex\n",
    );
    let cfg = CloudConfig::load_from(&path);
    assert!(cfg.effective_convex_url().is_empty());
    assert!(!cfg.cloud_relay_enabled());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn self_hosted_url_is_the_fallback_when_pairing_convex_url_is_empty() {
    // The "pairs but never beacons" case: a self_hosted posture whose URL
    // only landed in server.self_hosted.url (no pairing.convex_url). The
    // relay must still resolve a URL and beacon.
    let yaml = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://convex-site.operator.example
";
    let path = temp_yaml("selfhosted-fallback", yaml);
    let cfg = CloudConfig::load_from(&path);
    assert!(cfg.cloud_relay_enabled());
    assert_eq!(
        cfg.effective_convex_url(),
        "https://convex-site.operator.example"
    );
    let _ = std::fs::remove_file(&path);

    // pairing.convex_url still wins when both are present.
    let yaml2 = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://fallback.example
pairing:
  convex_url: https://primary.example
";
    let path2 = temp_yaml("selfhosted-primary-wins", yaml2);
    let cfg2 = CloudConfig::load_from(&path2);
    assert_eq!(cfg2.effective_convex_url(), "https://primary.example");
    let _ = std::fs::remove_file(&path2);

    // A cloud posture does NOT borrow the self_hosted URL (the fallback is
    // self_hosted-only): empty pairing.convex_url under cloud → empty.
    let yaml3 = "\
server:
  mode: cloud
  self_hosted:
    url: https://should-not-be-used.example
";
    let path3 = temp_yaml("cloud-no-selfhosted-borrow", yaml3);
    let cfg3 = CloudConfig::load_from(&path3);
    assert!(cfg3.effective_convex_url().is_empty());
    let _ = std::fs::remove_file(&path3);
}

#[test]
fn an_auto_profile_resolves_through_profile_conf() {
    // A default install leaves `agent.profile: auto` and records the node's
    // profile in profile.conf; a ground station must advertise itself as one.
    let conf = temp_yaml("profile-conf-gs", "# header\nprofile: ground_station\n");
    let mut cfg = CloudConfig::default();
    assert_eq!(cfg.agent.profile, "auto");
    assert_eq!(cfg.wire_profile_at(&conf), "ground-station");
    // An explicit config value still wins over profile.conf.
    cfg.agent.profile = "drone".to_string();
    assert_eq!(cfg.wire_profile_at(&conf), "drone");
    let _ = std::fs::remove_file(&conf);
    // No profile.conf: `auto` is a drone.
    cfg.agent.profile = "auto".to_string();
    assert_eq!(cfg.wire_profile_at(&conf), "drone");
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<CloudConfig>("");
}
