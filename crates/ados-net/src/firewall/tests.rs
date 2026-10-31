use super::testing::FakeNet;
use super::*;

struct FixedBackend(FirewallBackend);
impl BackendDetector for FixedBackend {
    fn detect(&self) -> FirewallBackend {
        self.0
    }
}

fn fw(net: Arc<FakeNet>, backend: FirewallBackend, dir: &std::path::Path) -> ShareUplinkFirewall {
    ShareUplinkFirewall::with_parts(
        net,
        Arc::new(FixedBackend(backend)),
        dir.join("99-ados-share-uplink.conf"),
        dir.join("rules.v4"),
        dir.join("nftables.d").join("ados-nat.nft"),
    )
}

fn intent(share: bool, active: &str, cap: DataCapState) -> FirewallIntent {
    FirewallIntent {
        share_uplink: share,
        active_iface: Some(active.to_string()),
        cellular_iface: Some("wwan0".to_string()),
        cap_state: cap,
    }
}

#[tokio::test]
async fn share_uplink_follows_the_active_uplink_and_leaves_no_stale_rule() {
    for backend in [
        FirewallBackend::IptablesPersistent,
        FirewallBackend::Nftables,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let net = Arc::new(FakeNet::default());
        let f = fw(Arc::clone(&net), backend, dir.path());
        let res = f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
        assert_eq!(res["applied"], true, "{res}");
        assert_eq!(net.nat_ifaces(), vec!["eth0"]);
        let dropin = std::fs::read_to_string(dir.path().join("99-ados-share-uplink.conf")).unwrap();
        assert!(dropin.contains("net.ipv4.ip_forward=1"));

        f.reconcile(intent(true, "wlan0", DataCapState::Ok)).await;
        assert_eq!(net.nat_ifaces(), vec!["wlan0"], "{backend:?}");

        f.reconcile(intent(false, "wlan0", DataCapState::Ok)).await;
        assert!(net.nat_ifaces().is_empty(), "{backend:?}");
        assert!(!dir.path().join("99-ados-share-uplink.conf").exists());
    }
}

#[tokio::test]
async fn cap_transitions_never_add_nat_while_sharing_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesPersistent,
        dir.path(),
    );
    for cap in [
        DataCapState::Ok,
        DataCapState::Warn80,
        DataCapState::Blocked100,
        DataCapState::Ok,
    ] {
        f.reconcile(intent(false, "wwan0", cap)).await;
        assert!(net.nat_ifaces().is_empty(), "NAT added at {cap:?}");
    }
}

#[tokio::test]
async fn the_cap_block_drops_nat_only_on_the_cellular_uplink() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(Arc::clone(&net), FirewallBackend::Nftables, dir.path());
    // Wired uplink active while the cellular cap is exhausted: the wired
    // share stays.
    f.reconcile(intent(true, "eth0", DataCapState::Blocked100))
        .await;
    assert_eq!(net.nat_ifaces(), vec!["eth0"]);
    // Failover onto the blocked cellular link: no NAT through it.
    f.reconcile(intent(true, "wwan0", DataCapState::Blocked100))
        .await;
    assert!(net.nat_ifaces().is_empty());
    // A new month lifts the block.
    f.reconcile(intent(true, "wwan0", DataCapState::Ok)).await;
    assert_eq!(net.nat_ifaces(), vec!["wwan0"]);
}

#[tokio::test]
async fn cap_shaping_survives_failover_away_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesRuntime,
        dir.path(),
    );
    f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
    f.reconcile(intent(false, "eth0", DataCapState::Throttle95))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
    assert_eq!(net.tbf_rate("eth0"), None);
    f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
    // A qdisc lost behind the daemon's back is restored by the next pass.
    net.clear_qdisc("wwan0");
    f.reconcile(intent(false, "wwan0", DataCapState::Throttle95))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), Some(THROTTLE_RATE_KBPS_95));
    // Back under the cap: unshaped.
    f.reconcile(intent(false, "wwan0", DataCapState::Warn80))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), None);
}

#[tokio::test]
async fn the_cap_block_holds_the_cellular_link_to_a_trickle() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesRuntime,
        dir.path(),
    );
    f.reconcile(intent(false, "eth0", DataCapState::Blocked100))
        .await;
    assert_eq!(net.tbf_rate("wwan0"), Some(BLOCKED_RATE_KBPS));
}

#[tokio::test]
async fn a_modem_iface_change_moves_the_shaping() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesRuntime,
        dir.path(),
    );
    f.reconcile(intent(false, "eth0", DataCapState::Throttle95))
        .await;
    let mut moved = intent(false, "eth0", DataCapState::Throttle95);
    moved.cellular_iface = Some("usb0".to_string());
    f.reconcile(moved).await;
    assert_eq!(net.tbf_rate("wwan0"), None);
    assert_eq!(net.tbf_rate("usb0"), Some(THROTTLE_RATE_KBPS_95));
}

#[tokio::test]
async fn a_steady_state_reconcile_rewrites_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesPersistent,
        dir.path(),
    );
    f.reconcile(intent(true, "eth0", DataCapState::Throttle95))
        .await;
    let before = net.calls().len();
    f.reconcile(intent(true, "eth0", DataCapState::Throttle95))
        .await;
    let mutating: Vec<_> = net.calls()[before..]
        .iter()
        .filter(|c| {
            c.iter().any(|a| a == "-F" || a == "add" || a == "del")
                || c.first().is_some_and(|a| a == "iptables-save")
        })
        .cloned()
        .collect();
    assert!(mutating.is_empty(), "steady state mutated: {mutating:?}");
}

#[tokio::test]
async fn nft_persists_only_the_ados_table() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(Arc::clone(&net), FirewallBackend::Nftables, dir.path());
    f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
    let body = std::fs::read_to_string(dir.path().join("nftables.d").join("ados-nat.nft")).unwrap();
    assert!(body.starts_with("#!/usr/sbin/nft -f\n"));
    assert!(body.contains("table ip ados_nat\ndelete table ip ados_nat\n"));
    assert!(body.contains("oifname \"eth0\" masquerade"));
    assert!(!net.calls().iter().any(|c| c.iter().any(|a| a == "ruleset")));
}

#[tokio::test]
async fn sharing_with_no_backend_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(net, FirewallBackend::None, dir.path());
    let res = f.reconcile(intent(true, "eth0", DataCapState::Ok)).await;
    assert_eq!(res["applied"], false);
    assert_eq!(res["backend"], "none");
    assert!(res["apply_error"]
        .as_str()
        .unwrap()
        .contains("no_firewall_backend"));
}

#[tokio::test]
async fn set_share_uplink_keeps_the_last_cap_state() {
    let dir = tempfile::tempdir().unwrap();
    let net = Arc::new(FakeNet::default());
    let f = fw(
        Arc::clone(&net),
        FirewallBackend::IptablesRuntime,
        dir.path(),
    );
    f.reconcile(intent(false, "wwan0", DataCapState::Blocked100))
        .await;
    // The operator turns sharing on while the cellular uplink is blocked.
    f.set_share_uplink(true, Some("wwan0")).await;
    assert!(net.nat_ifaces().is_empty());
    assert_eq!(net.tbf_rate("wwan0"), Some(BLOCKED_RATE_KBPS));
}

#[test]
fn parse_root_tbf_reads_the_kbit_rate() {
    assert_eq!(
        parse_root_tbf("qdisc tbf 8001: root refcnt 2 rate 256Kbit burst 4Kb lat 400ms\n"),
        Some(Some(256))
    );
    assert_eq!(
        parse_root_tbf("qdisc tbf 8001: root refcnt 2 rate 2Mbit burst 4Kb lat 400ms\n"),
        Some(None)
    );
    assert_eq!(parse_root_tbf("qdisc fq_codel 0: root refcnt 2\n"), None);
}
