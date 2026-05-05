//! Hardware-check engine.
//!
//! v0.1 stub — returns a minimal canonical-shape response so the wizard
//! does not break. The full board.yaml + /proc + lsusb fingerprint matcher
//! lands in B7.6.

use serde_json::json;

/// Snapshot the wizard consumes. Real impl in B7.6 will populate the
/// `components` array per the active profile's requirements.
pub fn run_hardware_check(profile: &str, ground_role: &str) -> serde_json::Value {
    let _ = ground_role;
    let _ = profile;
    // Canonical shape mirrors HardwareCheckStatus from the Python ref. v1
    // populates: components (list of {name, status, detail}), overall_ok,
    // missing_required (list of names). v0.1 returns a placeholder so the
    // wizard renders without errors.
    json!({
        "overall_ok": true,
        "components": [
            { "name": "flight_controller", "status": "unknown", "detail": "v0.1 stub; full check in B7.6" },
            { "name": "camera",            "status": "unknown", "detail": "v0.1 stub; full check in B7.6" },
            { "name": "wifi",              "status": "unknown", "detail": "v0.1 stub; full check in B7.6" }
        ],
        "missing_required": [],
        "stub": true
    })
}
