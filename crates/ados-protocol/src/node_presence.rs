//! The `gnss` and `lora` node capability flags, shared by the LAN
//! `/api/status/full` route and the cloud heartbeat so both transports report
//! the same answer for the same node.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Freshness window of the 1 Hz `gnss.json` sidecar (mirrors `contracts.toml`).
pub const GNSS_FRESH: Duration = Duration::from_secs(5);

fn unit_on(unit: &str, profile: &str) -> bool {
    let profile = profile.replace('-', "_");
    crate::units::find(unit).is_some_and(|u| u.on_profile(&profile))
}

/// The GNSS lane works: `ados-gnss` runs on this profile (wire or underscore
/// form) and its sidecar is fresh. The service writes it every second only
/// while receivers are configured, so a file left by a stopped service goes
/// stale and does not count.
pub fn gnss_present(profile: &str, run_dir: &Path, now: SystemTime) -> bool {
    unit_on("ados-gnss", profile)
        && crate::sidecar::read_fresh_at::<serde_json::Value>(
            &run_dir.join("gnss.json"),
            Some(GNSS_FRESH),
            now,
        )
        .is_fresh()
}

/// The LoRa lane is enabled: `ados-lora` runs on this profile and the
/// `lora-enabled` marker sits in the config directory (`etc_dir`).
pub fn lora_present(profile: &str, etc_dir: &Path) -> bool {
    unit_on("ados-lora", profile) && etc_dir.join("lora-enabled").exists()
}
