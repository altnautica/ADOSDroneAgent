//! Contract-E filesystem path constants used by the uplink matrix.
//!
//! Mirrors the subset of `core/paths.py` the uplink router reads and writes.
//! Other crates (mesh) read `UPLINK_ACTIVE_FLAG` by literal path, so the
//! constants here must stay byte-identical to the Python side.

use std::path::Path;

/// Runtime directory (`ADOS_RUN_DIR`). tmpfs; wiped on reboot.
pub const ADOS_RUN_DIR: &str = "/run/ados";

/// Persistent config directory (`ADOS_ETC_DIR`).
pub const ADOS_ETC_DIR: &str = "/etc/ados";

/// Persisted uplink priority list (`GS_UPLINK_JSON`). Owner-written JSON
/// `{"priority": [...]}`.
pub const GS_UPLINK_JSON: &str = "/etc/ados/ground-station-uplink.json";

/// Active-uplink sentinel (`UPLINK_ACTIVE_FLAG`). The mesh gateway-election
/// path reads this by `.is_file()` to decide whether a node can advertise
/// itself as a cloud gateway, so the router unlinks it when no uplink is
/// viable. We additionally write a JSON body for richer consumers.
pub const UPLINK_ACTIVE_FLAG: &str = "/run/ados/uplink-active";

/// `usb0` carrier sysfs path. The USB-tether check reads this when no
/// dedicated manager is wired.
pub const USB0_CARRIER: &str = "/sys/class/net/usb0/carrier";

/// Build the canonical `GS_UPLINK_JSON` path.
pub fn gs_uplink_json() -> &'static Path {
    Path::new(GS_UPLINK_JSON)
}

/// Build the canonical `UPLINK_ACTIVE_FLAG` path.
pub fn uplink_active_flag() -> &'static Path {
    Path::new(UPLINK_ACTIVE_FLAG)
}
