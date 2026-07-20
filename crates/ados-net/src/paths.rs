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

/// Device-id file (`DEVICE_ID_PATH`). The AP SSID derives its short suffix from
/// the first four hex chars of this id.
pub const DEVICE_ID_PATH: &str = "/etc/ados/device-id";

/// hostapd config (`HOSTAPD_CONF_PATH`), written 0600.
pub const HOSTAPD_CONF_PATH: &str = "/etc/ados/hostapd-gs.conf";

/// AP dnsmasq config (`DNSMASQ_CONF_PATH`), written 0644.
pub const DNSMASQ_CONF_PATH: &str = "/etc/ados/dnsmasq-gs.conf";

/// AP passphrase file (`AP_PASSPHRASE_PATH`), written 0600 + trailing newline.
pub const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";

/// USB-gadget dnsmasq runtime config (`DNSMASQ_USB0_CONF`).
pub const DNSMASQ_USB0_CONF: &str = "/run/ados/dnsmasq-usb0.conf";

/// USB-gadget dnsmasq pid file (`DNSMASQ_USB0_PID`).
pub const DNSMASQ_USB0_PID: &str = "/run/ados/dnsmasq-usb0.pid";

/// Cellular modem config sidecar (`GS_MODEM_JSON`). Owner-written JSON
/// `{"apn":..,"cap_gb":..,"enabled":..}`.
pub const GS_MODEM_JSON: &str = "/etc/ados/ground-station-modem.json";

/// AP-was-enabled handoff flag (`AP_FLAG_PATH`). The wifi-client manager writes
/// it when it stops the AP to take `wlan0` for a station connection and clears
/// it on `leave`, so its presence means a client join owns the radio. The
/// setup-AP guard reads it (by `.exists()`) to avoid racing the join/leave path.
pub const AP_WAS_ENABLED_FLAG: &str = "/run/ados/ap-was-enabled";

/// Setup-AP guard decision sidecar (`AP_GUARD_JSON`). The `ados-net` daemon
/// writes the live stand-down decision here each reconcile; the control front
/// merges it into the ground-station AP status so the decision is diagnosable.
pub const AP_GUARD_JSON: &str = "/run/ados/ap-guard.json";

/// Build the canonical `GS_UPLINK_JSON` path.
pub fn gs_uplink_json() -> &'static Path {
    Path::new(GS_UPLINK_JSON)
}

/// Build the canonical `UPLINK_ACTIVE_FLAG` path.
pub fn uplink_active_flag() -> &'static Path {
    Path::new(UPLINK_ACTIVE_FLAG)
}

/// Operator command socket for the WiFi-client uplink (`WIFI_CMD_SOCK`). The
/// REST `/network/client/join`/`forget` handlers forward to this when the native
/// daemon owns the uplink, so they never drive `nmcli` on `wlan0` in-process and
/// race the daemon's WiFi manager. Mirrors the radio's `wfb-cmd.sock`.
pub const WIFI_CMD_SOCK: &str = "/run/ados/wifi-cmd.sock";

/// Build the canonical `WIFI_CMD_SOCK` path.
pub fn wifi_cmd_sock() -> &'static Path {
    Path::new(WIFI_CMD_SOCK)
}
