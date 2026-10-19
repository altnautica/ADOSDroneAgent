//! Ground-station adapter resolution, shared by the direct, relay and receiver
//! roles: detect and verify every WFB adapter, assign roles from the
//! `radio.adapters` config, and hand the mesh-role adapter back to the kernel
//! for the mesh carrier.

use std::path::Path;

use ados_radio::adapter::{AdapterPlan, DetectedAdapter, DetectionOutcome};
use ados_radio::config::RadioAdaptersConfig;

/// What one resolution pass found.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// At least one adapter verified and can carry WFB.
    Ready(AdapterPlan),
    /// Candidates were found but none verified monitor mode. Carries the best
    /// ranked one so the sidecar can say why (typically a slow USB link).
    NoInjection(DetectedAdapter),
    /// No WFB adapter to use.
    NoAdapter,
}

/// Classify a detection pass. Pure so every arm is testable without radios.
pub fn classify(
    outcome: &DetectionOutcome,
    cfg: &RadioAdaptersConfig,
    mesh_enabled: bool,
) -> Resolution {
    let verified = outcome.verified();
    if verified.is_empty() {
        return match outcome.candidates.first() {
            Some(first) => Resolution::NoInjection(first.clone()),
            None => Resolution::NoAdapter,
        };
    }
    match AdapterPlan::new(ados_radio::adapter::resolve_roles(
        &verified,
        cfg,
        mesh_enabled,
    )) {
        Some(plan) => Resolution::Ready(plan),
        None => {
            tracing::warn!(
                verified = verified.len(),
                "ground_wfb_no_wfb_role: every verified adapter is assigned to the mesh"
            );
            Resolution::NoAdapter
        }
    }
}

/// Detect, verify and resolve the adapters. `override_iface` is the operator's
/// `video.wfb.interface` pin (moved to the front when it passes the gate);
/// `mesh_enabled` is true for the roles that run the mesh carrier (relay and
/// receiver). A mesh-role adapter is returned to managed mode, since the
/// verification pass put it into monitor mode and the mesh carrier needs it
/// back.
pub async fn resolve(override_iface: &str, mesh_enabled: bool) -> Resolution {
    let outcome = ados_radio::adapter::detect_all(override_iface).await;
    let cfg = RadioAdaptersConfig::load_from(Path::new(crate::paths::CONFIG_YAML));
    let resolution = classify(&outcome, &cfg, mesh_enabled);
    if let Resolution::Ready(plan) = &resolution {
        for a in plan.adapters() {
            tracing::info!(
                interface = %a.ifname(),
                role = a.role.as_str(),
                antenna = ?a.antenna,
                "ground_wfb_adapter_role"
            );
        }
        if let Some(mesh) = plan.mesh() {
            ados_radio::adapter::set_managed_mode(mesh.ifname()).await;
        }
    }
    resolution
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(i: u8, verified: bool) -> DetectedAdapter {
        DetectedAdapter {
            ifname: format!("wlan{i}"),
            mac: None,
            usb_path: None,
            driver: "rtl88x2eu".into(),
            chipset: "RTL8812EU".into(),
            verified,
            usb_speed_mbps: Some(12),
            usb_degraded: true,
        }
    }

    fn outcome(candidates: Vec<DetectedAdapter>) -> DetectionOutcome {
        DetectionOutcome {
            scanned: Vec::new(),
            candidates,
        }
    }

    #[test]
    fn no_candidates_is_no_adapter() {
        let r = classify(&outcome(vec![]), &RadioAdaptersConfig::default(), false);
        assert!(matches!(r, Resolution::NoAdapter));
    }

    #[test]
    fn unverified_candidates_report_the_first_one() {
        let r = classify(
            &outcome(vec![candidate(1, false), candidate(2, false)]),
            &RadioAdaptersConfig::default(),
            false,
        );
        match r {
            Resolution::NoInjection(a) => assert_eq!(a.ifname, "wlan1"),
            other => panic!("expected NoInjection, got {other:?}"),
        }
    }

    #[test]
    fn only_verified_adapters_enter_the_plan() {
        let r = classify(
            &outcome(vec![
                candidate(1, false),
                candidate(2, true),
                candidate(3, true),
            ]),
            &RadioAdaptersConfig::default(),
            false,
        );
        match r {
            Resolution::Ready(plan) => {
                assert_eq!(plan.tx().ifname(), "wlan2");
                assert_eq!(plan.rx_ifaces(), vec!["wlan2", "wlan3"]);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }
}
