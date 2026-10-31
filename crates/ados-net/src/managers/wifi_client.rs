//! WiFi client (station) manager for the ground-station profile.
//!
//! The onboard management radio is either an AP (hostapd) or a station joining
//! an upstream network for backhaul; the two are mutually exclusive on one
//! radio. This manager coordinates with the AP service through an advisory file
//! lock and a "was the AP up?" flag so a failed join restores the AP.
//! NetworkManager owns credential storage; we persist only the "enabled on
//! boot" flag and the last SSID. Implements the [`UplinkManager`] trait.
//!
//! The interface is resolved by DRIVER, never assumed to be `wlan0`: names race
//! at boot, and a station bound to the wrong name runs `nmcli` on the aircraft's
//! flight radio. The lock file is named after the resolved interface for the
//! same reason.
//!
//! RISK: the station lock must live across the whole `stop-hostapd → nmcli
//! connect` window and be released only by [`WifiClientManager::leave`] (or a
//! failed join's cleanup). It is held in the manager struct, not taken and
//! dropped per call.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tracing::warn;

use crate::cmd::CmdRunner;
use crate::router::UplinkManager;

use self::lock::Wlan0Lock;

mod join;
mod profiles;

const HOSTAPD_UNIT: &str = "ados-hostapd.service";
const LOCK_DIR: &str = "/var/lock";
const AP_FLAG_PATH: &str = crate::paths::AP_WAS_ENABLED_FLAG;
const CLIENT_CONFIG_PATH: &str = "/etc/ados/ground-station-wifi-client.json";

const RUN_TIMEOUT: Duration = Duration::from_secs(15);
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// Persisted client config. Field order matches the Python dict insertion
/// order so `to_string_pretty` byte-matches `json.dumps(indent=2)`.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub enabled_on_boot: bool,
    #[serde(default)]
    pub last_ssid: Option<String>,
}

/// The advisory lock file name for a station interface.
///
/// Derived from the interface rather than fixed at `ados-wlan0.lock`: the lock
/// only keeps hostapd off the radio while both holders name the same file, so a
/// lock named for `wlan0` held while the station drives `wlan1` excludes nobody.
fn lock_file_name(iface: &str) -> String {
    format!("ados-{iface}.lock")
}

/// [`lock_file_name`] under the system lock directory.
fn lock_path_for(iface: &str) -> PathBuf {
    PathBuf::from(LOCK_DIR).join(lock_file_name(iface))
}

/// Pick the station interface out of an already-enumerated wireless set.
///
/// The station and the access point contend for ONE radio, so both resolve from
/// the same inputs by the same rule (`choose_ap_interface`, the pure core of
/// `resolve_ap_interface`). Measured across three reboots of a ground station,
/// `wlan0` was the onboard chip twice and the USB long-range radio once, so a
/// name-bound station would have driven the flight link one boot in three.
///
/// `None` when no onboard (non-radio) WiFi resolves: there is then no station
/// interface, and `join` refuses rather than guessing a name.
fn station_iface_from(
    ifaces: &[ados_protocol::netif::WirelessIface],
    configured: &str,
    radio_iface: Option<&str>,
) -> Option<String> {
    match ados_protocol::netif::choose_ap_interface(ifaces, configured, radio_iface) {
        Ok(iface) => Some(iface),
        Err(exc) => {
            warn!(error = %exc, "station_iface_unresolved");
            None
        }
    }
}

/// [`station_iface_from`] against the live `/sys/class/net`, the operator's pin,
/// and the interface the radio itself reports it took.
fn station_interface() -> Option<String> {
    station_iface_from(
        &ados_protocol::netif::list_wireless(),
        &ados_protocol::ap_country::configured_ap_interface(),
        ados_protocol::netif::radio_interface().as_deref(),
    )
}

/// NetworkManager-backed WiFi station manager for the onboard management radio.
pub struct WifiClientManager {
    /// The resolved station interface; empty when none resolved.
    interface: String,
    runner: Arc<dyn CmdRunner>,
    lock_path: PathBuf,
    ap_flag_path: PathBuf,
    client_config_path: PathBuf,
    /// The held station advisory lock. `Some` while we own the radio for STA mode
    /// across the stop-hostapd → connect window; dropped on `leave`.
    lock: Option<Wlan0Lock>,
}

impl WifiClientManager {
    /// Manager with canonical paths and the production runner.
    ///
    /// Resolves the radio at construction, the same point `EthernetManager` and
    /// the AP guard resolve theirs, so a boot-time udev race cannot leave the
    /// station pointed at whatever enumerated as `wlan0` this time.
    pub fn new(runner: Arc<dyn CmdRunner>) -> Self {
        let interface = station_interface().unwrap_or_default();
        let lock_path = lock_path_for(&interface);
        Self::with_paths(
            interface,
            runner,
            lock_path,
            PathBuf::from(AP_FLAG_PATH),
            PathBuf::from(CLIENT_CONFIG_PATH),
        )
    }

    /// Full constructor (tests).
    pub fn with_paths(
        interface: impl Into<String>,
        runner: Arc<dyn CmdRunner>,
        lock_path: PathBuf,
        ap_flag_path: PathBuf,
        client_config_path: PathBuf,
    ) -> Self {
        Self {
            interface: interface.into(),
            runner,
            lock_path,
            ap_flag_path,
            client_config_path,
            lock: None,
        }
    }

    /// True while the manager holds the station advisory lock.
    pub fn holds_lock(&self) -> bool {
        self.lock.is_some()
    }

    // ---------------- lock handling ----------------

    /// Acquire the exclusive non-blocking station lock and stash it in the
    /// struct. Returns false when another holder owns it.
    fn acquire_lock(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        match Wlan0Lock::try_acquire(&self.lock_path) {
            Ok(lk) => {
                self.lock = Some(lk);
                true
            }
            Err(exc) => {
                warn!(error = %exc, "station_lock_failed");
                false
            }
        }
    }

    /// Drop the held lock (LOCK_UN + close happen in `Wlan0Lock::drop`).
    fn release_lock(&mut self) {
        self.lock = None;
    }

    async fn is_hostapd_active(&self) -> bool {
        let out = self
            .runner
            .run(&["systemctl", "is-active", HOSTAPD_UNIT], SHORT_TIMEOUT)
            .await;
        out.stdout.trim() == "active"
    }

    fn write_ap_flag(&self, enabled: bool) {
        if let Some(parent) = self.ap_flag_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = if enabled { "1\n" } else { "0\n" };
        if let Err(exc) = std::fs::write(&self.ap_flag_path, body) {
            warn!(error = %exc, "ap_flag_write_failed");
        }
    }

    fn read_ap_flag(&self) -> bool {
        std::fs::read_to_string(&self.ap_flag_path)
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    fn clear_ap_flag(&self) {
        match std::fs::remove_file(&self.ap_flag_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(exc) => warn!(error = %exc, "ap_flag_clear_failed"),
        }
    }

    // ---------------- client config ----------------

    fn load_client_config(&self) -> ClientConfig {
        match std::fs::read(&self.client_config_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => ClientConfig::default(),
        }
    }

    /// Persist `{enabled_on_boot, last_ssid}` as `json.dumps(indent=2)+"\n"`,
    /// atomically (tmp `.json.tmp` sibling + rename).
    fn save_client_config(&self, data: &ClientConfig) {
        let body = match serde_json::to_string_pretty(data) {
            Ok(s) => format!("{s}\n"),
            Err(exc) => {
                warn!(error = %exc, "client_config_encode_failed");
                return;
            }
        };
        if let Err(exc) = crate::sidecar::write_atomic(&self.client_config_path, body.as_bytes()) {
            warn!(error = %exc, "client_config_write_failed");
        }
    }

    /// Set the "rejoin on boot" flag, preserving the last SSID.
    pub async fn set_enabled_on_boot(&self, enabled: bool) -> ClientConfig {
        let mut data = self.load_client_config();
        data.enabled_on_boot = enabled;
        self.save_client_config(&data);
        data
    }

    // ---------------- public API ----------------
}

#[async_trait]
impl UplinkManager for WifiClientManager {
    async fn is_up(&self) -> bool {
        self.status()
            .await
            .get("connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
    fn get_iface(&self) -> String {
        self.interface.clone()
    }
    async fn get_gateway(&self) -> Option<String> {
        self.status()
            .await
            .get("gateway")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

fn err_or(stderr: &str, fallback: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// The advisory wlan0 lock. On Linux it is a real `flock(LOCK_EX|LOCK_NB)` held
/// on an open fd via `nix::fcntl::Flock`; the lock is released (`LOCK_UN`) and
/// the fd closed when the value is dropped. On non-Linux dev hosts an
/// equivalent `flock` is taken through a tiny libc FFI so the lock-lifetime
/// behavior is the same and testable.
mod lock;

#[cfg(test)]
mod tests;
