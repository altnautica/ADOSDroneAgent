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
mod tests;
