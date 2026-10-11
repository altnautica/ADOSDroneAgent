use super::*;

#[test]
fn ladder_drops_wifi_rung_on_ethernet() {
    let eth = ladder_for(HealthVerdict::Down, Transport::Ethernet);
    assert_eq!(
        eth,
        vec![
            RepairRung::ReassertReg,
            RepairRung::RenewDhcp,
            RepairRung::BounceIface,
            RepairRung::RestartBackend,
        ]
    );
    let wifi = ladder_for(HealthVerdict::Degraded, Transport::Wifi);
    assert_eq!(
        wifi,
        vec![
            RepairRung::ReassertReg,
            RepairRung::RenewDhcp,
            RepairRung::ReconnectWifi,
            RepairRung::BounceIface,
            RepairRung::RestartBackend,
        ]
    );
}

#[test]
fn ladder_always_starts_with_the_cheapest_rung() {
    for v in [HealthVerdict::Degraded, HealthVerdict::Down] {
        for t in [Transport::Ethernet, Transport::Wifi] {
            assert_eq!(ladder_for(v, t)[0], RepairRung::ReassertReg);
        }
    }
}

#[test]
fn rung_drop_classification() {
    assert!(!rung_drops_link(RepairRung::ReassertReg));
    assert!(!rung_drops_link(RepairRung::RenewDhcp));
    assert!(rung_drops_link(RepairRung::ReconnectWifi));
    assert!(rung_drops_link(RepairRung::BounceIface));
    assert!(rung_drops_link(RepairRung::RestartBackend));
}

#[test]
fn live_control_path_test() {
    assert!(is_live_control_path("eth0", Some("eth0")));
    assert!(!is_live_control_path("eth0", Some("wlan0")));
    assert!(!is_live_control_path("eth0", None));
}

#[test]
fn rung_strings_are_bland() {
    assert_eq!(RepairRung::ReassertReg.as_str(), "reassert_reg");
    assert_eq!(RepairRung::Exhausted.as_str(), "exhausted");
}

#[test]
fn detail_builders_carry_bland_fields() {
    let h = link_health_detail(
        "eth0",
        Transport::Ethernet,
        LinkSignals {
            carrier: true,
            has_lease: true,
            gateway_reachable: false,
        },
        HealthVerdict::Degraded,
    );
    assert_eq!(h.get("interface").and_then(|v| v.as_str()), Some("eth0"));
    assert_eq!(h.get("state").and_then(|v| v.as_str()), Some("degraded"));
    assert_eq!(
        h.get("gateway_reachable").and_then(|v| v.as_bool()),
        Some(false)
    );

    let r = repair_attempt_detail("wlan0", "networkd", RepairRung::BounceIface, true);
    assert_eq!(r.get("rung").and_then(|v| v.as_str()), Some("bounce_iface"));
    assert_eq!(r.get("backend").and_then(|v| v.as_str()), Some("networkd"));
    assert_eq!(
        r.get("self_restoring").and_then(|v| v.as_bool()),
        Some(true)
    );

    let e = repair_exhausted_detail("eth0", 5);
    assert_eq!(e.get("repairs_in_window").and_then(|v| v.as_u64()), Some(5));
}
