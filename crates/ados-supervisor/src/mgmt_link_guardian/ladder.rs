//! Pure repair-ladder policy + log-event detail builders for the guardian.
//!
//! No OS calls: the ladder ordering, the link-drop classification, the
//! live-control-path test, and the event detail maps are all pure and
//! unit-tested on every host. Event kinds and field names are bland and
//! reader-facing (no internal tags).

use ados_protocol::logd::{Fields, Level, Value as MpVal};

use super::detection::{HealthVerdict, LinkSignals, Transport};

/// Event kind for a management-link health-state transition.
pub const LINK_HEALTH_KIND: &str = "network.link_health_check";
/// Event kind for one repair-rung attempt.
pub const LINK_REPAIR_KIND: &str = "network.link_repair_attempt";
/// Event kind when the ladder is exhausted (every rung tried, link still dead).
pub const LINK_EXHAUSTED_KIND: &str = "network.link_repair_exhausted";

/// One rung of the escalating, idempotent repair ladder. Ordered cheapest →
/// most disruptive. `Exhausted` is the terminal marker (every rung tried, the
/// link still dead) that hands off to the reach-back layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairRung {
    /// Re-assert the global regulatory domain — the most common cause (a foreign
    /// baked country from the injection radio breaking the onboard data path).
    /// Interface-agnostic, channel-safety-gated, never drops the link.
    ReassertReg,
    /// Renew the DHCP lease on the managed interface.
    RenewDhcp,
    /// Re-associate the onboard Wi-Fi (Wi-Fi transport only).
    ReconnectWifi,
    /// Bounce the managed interface (atomic down→up).
    BounceIface,
    /// Restart the network backend daemon.
    RestartBackend,
    /// Every rung tried and the link is still dead.
    Exhausted,
}

impl RepairRung {
    /// The bland, reader-facing string for this rung (event detail + sidecar).
    pub fn as_str(self) -> &'static str {
        match self {
            RepairRung::ReassertReg => "reassert_reg",
            RepairRung::RenewDhcp => "renew_dhcp",
            RepairRung::ReconnectWifi => "reconnect_wifi",
            RepairRung::BounceIface => "bounce_iface",
            RepairRung::RestartBackend => "restart_backend",
            RepairRung::Exhausted => "exhausted",
        }
    }
}

/// The ordered rungs to climb for one repair episode, given the verdict and
/// transport. Both `Degraded` and `Down` start at the cheapest rung; the
/// Wi-Fi-only reconnect is dropped on a wired link. Pure.
pub fn ladder_for(_verdict: HealthVerdict, transport: Transport) -> Vec<RepairRung> {
    let mut rungs = vec![RepairRung::ReassertReg, RepairRung::RenewDhcp];
    if transport == Transport::Wifi {
        rungs.push(RepairRung::ReconnectWifi);
    }
    rungs.push(RepairRung::BounceIface);
    rungs.push(RepairRung::RestartBackend);
    rungs
}

/// Whether a rung momentarily drops the managed link. Such rungs are issued as
/// an atomic local down→up the supervisor (a local daemon) completes without the
/// operator's link being alive. Pure.
pub fn rung_drops_link(rung: RepairRung) -> bool {
    matches!(
        rung,
        RepairRung::ReconnectWifi | RepairRung::BounceIface | RepairRung::RestartBackend
    )
}

/// Whether the managed interface IS the operator's live control path right now
/// (it equals the current default-route interface). When true, link-dropping
/// rungs must be self-restoring. Pure.
pub fn is_live_control_path(managed: &str, current_default: Option<&str>) -> bool {
    current_default == Some(managed)
}

/// The log severity for a verdict: a non-healthy link is a warning.
pub fn level_for(verdict: HealthVerdict) -> Level {
    match verdict {
        HealthVerdict::Healthy => Level::Info,
        HealthVerdict::Degraded | HealthVerdict::Down => Level::Warn,
    }
}

/// The bland string for a verdict (event detail + sidecar `state`).
pub fn verdict_str(verdict: HealthVerdict) -> &'static str {
    match verdict {
        HealthVerdict::Healthy => "healthy",
        HealthVerdict::Degraded => "degraded",
        HealthVerdict::Down => "down",
    }
}

/// Build the `network.link_health_check` detail map (a state transition). Pure.
pub fn link_health_detail(
    iface: &str,
    transport: Transport,
    signals: LinkSignals,
    verdict: HealthVerdict,
) -> Fields {
    let mut d = Fields::new();
    d.insert("interface".to_string(), MpVal::from(iface));
    d.insert("transport".to_string(), MpVal::from(transport.as_str()));
    d.insert("state".to_string(), MpVal::from(verdict_str(verdict)));
    d.insert("carrier".to_string(), MpVal::from(signals.carrier));
    d.insert("has_lease".to_string(), MpVal::from(signals.has_lease));
    d.insert(
        "gateway_reachable".to_string(),
        MpVal::from(signals.gateway_reachable),
    );
    d
}

/// Build the `network.link_repair_attempt` detail map for one rung. Pure.
pub fn repair_attempt_detail(
    iface: &str,
    backend: &str,
    rung: RepairRung,
    dropping_on_control_path: bool,
) -> Fields {
    let mut d = Fields::new();
    d.insert("interface".to_string(), MpVal::from(iface));
    d.insert("backend".to_string(), MpVal::from(backend));
    d.insert("rung".to_string(), MpVal::from(rung.as_str()));
    d.insert(
        "self_restoring".to_string(),
        MpVal::from(dropping_on_control_path),
    );
    d
}

/// Build the `network.link_repair_exhausted` detail map. Pure.
pub fn repair_exhausted_detail(iface: &str, repairs_in_window: u32) -> Fields {
    let mut d = Fields::new();
    d.insert("interface".to_string(), MpVal::from(iface));
    d.insert(
        "repairs_in_window".to_string(),
        MpVal::from(repairs_in_window as u64),
    );
    d
}

#[cfg(test)]
mod tests {
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
}
