//! Ground-station cellular modem manager.
//!
//! D-Bus first via `org.freedesktop.ModemManager1`: enumerate modems through
//! the ObjectManager, enable the first one, and bring up a data session with
//! `Modem.Simple.Connect({apn})`. On three consecutive D-Bus failures the
//! manager flips to AT-fallback mode and recovers on the next D-Bus success.
//! Ports `modem_manager.py`.
//!
//! AT/serial path: when D-Bus is unavailable the manager drives the modem's AT
//! control port directly (see [`crate::managers::modem_at`]): open
//! `/dev/ttyUSB2` at 115200 8N1, run the AT bring-up sequence, wait for the
//! `usb0` netdev, and poll signal / technology / operator / imei over AT. The
//! serial port is opened only on a board that has actually flipped to fallback,
//! so a board with ModemManager never touches the serial path.
//!
//! The modem is HW-gated and DISABLED by default. It never auto-connects: the
//! daemon only brings it up when `/etc/ados/ground-station-modem.json` has
//! `enabled: true`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::cmd::{CmdRunner, TokioCmdRunner};
use crate::managers::modem_at::{self, SerialTransport};
use crate::router::UplinkManager;
use crate::sidecar;

mod session;
mod status;

const USB_IFACE: &str = "usb0";
/// Metric of the default route a static-IP bearer installs: below DHCP
/// defaults, above the router's active-uplink route (10).
const STATIC_ROUTE_METRIC: &str = "30";
const IP_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_FAIL_THRESHOLD: u32 = 3;
/// How often a manager in AT fallback tries D-Bus again on a bring-up.
const DBUS_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_APN_FALLBACK: &str = "internet";
/// configfs root + gadget name the daemon provisions the USB tether under. A
/// present gadget dir means `usb0` belongs to the tether, not the modem, so the
/// modem must NOT claim it as its cellular iface (mirrors `data_cap`'s carve-out
/// so a modem-less board with a tether never reports a cellular link up).
const USB_GADGET_DIR: &str = "/sys/kernel/config/usb_gadget/ados_gs";

// D-Bus-only constants. Referenced solely by the Linux `zbus_impl` module, so
// they are gated to the Linux target to stay dead-code-free on a dev host.
#[cfg(target_os = "linux")]
const DBUS_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12); // _DBUS_TIMEOUT * 4
#[cfg(target_os = "linux")]
const MM_SERVICE: &str = "org.freedesktop.ModemManager1";
#[cfg(target_os = "linux")]
const MM_ROOT_PATH: &str = "/org/freedesktop/ModemManager1";

/// IMSI MCC-MNC prefix → APN for Indian carriers (MCC 404/405). Kept small and
/// maintained rather than pulling a full mobile-network list. Verbatim from
/// `_IMSI_APN_MAP`.
pub const IMSI_APN_MAP: &[(&str, &str)] = &[
    // Jio (Reliance)
    ("405857", "jionet"),
    ("405854", "jionet"),
    ("405855", "jionet"),
    ("405856", "jionet"),
    ("405874", "jionet"),
    // Airtel
    ("40410", "airtelgprs.com"),
    ("40445", "airtelgprs.com"),
    ("40449", "airtelgprs.com"),
    ("40490", "airtelgprs.com"),
    ("40492", "airtelgprs.com"),
    ("40493", "airtelgprs.com"),
    ("40494", "airtelgprs.com"),
    ("40495", "airtelgprs.com"),
    ("40496", "airtelgprs.com"),
    ("40497", "airtelgprs.com"),
    ("40498", "airtelgprs.com"),
    // Vi (Vodafone Idea)
    ("40411", "portalnmms"),
    ("40443", "www"),
    ("40446", "www"),
    // BSNL
    ("40434", "bsnlnet"),
    ("40438", "bsnlnet"),
    ("40451", "bsnlnet"),
    ("40453", "bsnlnet"),
    ("40459", "bsnlnet"),
];

/// Resolve an APN from an IMSI by longest-matching the prefix table. Mirrors the
/// Python `startswith` scan (first match in declared order wins).
pub fn apn_for_imsi(imsi: &str) -> Option<&'static str> {
    IMSI_APN_MAP
        .iter()
        .find(|(prefix, _)| imsi.starts_with(prefix))
        .map(|(_, apn)| *apn)
}

/// An APN is 1-63 characters of `[A-Za-z0-9.-]`. It is spliced into an AT
/// command line on the AT fallback path, so nothing else is ever accepted.
pub fn validate_apn(apn: &str) -> Result<(), String> {
    let ok = (1..=63).contains(&apn.len())
        && apn
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err("APN must be 1-63 characters of letters, digits, '.' and '-'".to_string())
    }
}

/// The cellular session the daemon should drive given the persisted config.
/// The daemon owns the modem (the REST modem write path only persists the
/// config file), so it re-reads the file each poll and reconciles the live
/// session to this desired state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModemSession {
    /// Dial the data session (config file present AND `enabled` not false).
    Up,
    /// Tear the data session down (config file present AND `enabled` is false).
    Down,
    /// Leave the modem untouched (no config file → never auto-dial a bare board).
    Leave,
}

/// Decide the desired cellular session from the persisted config. A board with
/// no config file is left alone (`Leave`), so a bench box never auto-dials. With
/// a config file present, `enabled` (default true when the key is absent, to
/// match the Python `config.get("enabled", True)`) decides up vs down.
pub fn desired_modem_session(config_present: bool, cfg: &ModemConfig) -> ModemSession {
    if !config_present {
        return ModemSession::Leave;
    }
    if cfg.enabled.unwrap_or(true) {
        ModemSession::Up
    } else {
        ModemSession::Down
    }
}

/// Persisted modem config sidecar (`{apn, cap_gb, enabled}`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ModemConfig {
    #[serde(default)]
    pub apn: Option<String>,
    #[serde(default)]
    pub cap_gb: Option<f64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ModemConfig {
    /// Load the persisted modem config from `path`, falling back to defaults on
    /// any read/parse error (an absent sidecar is the no-modem-configured case).
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Render byte-identically to Python `json.dumps({apn, cap_gb, enabled})`
    /// (default `", "` / `": "` separators, key order apn → cap_gb → enabled,
    /// floats rendered the same way serde and Python both render them, no
    /// trailing newline). Only the present fields are emitted, matching the
    /// Python dict that only carries set keys.
    pub fn render_json(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(apn) = &self.apn {
            parts.push(format!("\"apn\": {}", json_str(apn)));
        }
        if let Some(cap) = self.cap_gb {
            parts.push(format!("\"cap_gb\": {}", json_num(cap)));
        }
        if let Some(enabled) = self.enabled {
            parts.push(format!("\"enabled\": {enabled}"));
        }
        format!("{{{}}}", parts.join(", "))
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn json_num(n: f64) -> String {
    serde_json::to_string(&n).unwrap_or_else(|_| "0".into())
}

/// Outcome of a D-Bus bring-up attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbusConnectResult {
    /// `Bearer.Interface`; empty when the bearer did not report one.
    pub iface: String,
    pub ip: String,
    pub apn: String,
    /// The bearer object path `Simple.Connect` returned.
    pub bearer: String,
    /// The bearer's IPv4 settings when ModemManager hands them to the host to
    /// apply (`Ip4Config.method == static`); `None` for DHCP / PPP bearers.
    pub ip4_static: Option<Ip4Static>,
}

/// A static IPv4 configuration a bearer asks the host to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ip4Static {
    pub address: String,
    pub prefix: u32,
    pub gateway: Option<String>,
}

/// The cellular data interface under `net_dir`, by driver and bus: any modem
/// data port other than `usb0` first, then `usb0` only when the USB-gadget
/// tether is not provisioned (a provisioned gadget owns `usb0`). The modem
/// manager and the data-cap source share this resolver.
pub fn resolve_cellular_iface(net_dir: &Path, gadget_dir: &Path) -> Option<String> {
    let found = ados_protocol::netif::list_cellular_in(net_dir);
    if let Some(name) = found.iter().find(|n| n.as_str() != USB_IFACE) {
        return Some(name.clone());
    }
    found
        .into_iter()
        .find(|n| n == USB_IFACE && !gadget_dir.exists())
}

/// The D-Bus seam. The production impl wraps zbus; tests inject a scripted fake
/// so the fail-threshold → fallback logic is exercised without a real bus. All
/// methods are best-effort and return `Err` on any bus/timeout/no-modem error
/// so the manager's failure counter advances uniformly.
#[async_trait]
pub trait ModemDbus: Send + Sync {
    /// Enable the first modem and connect a data session with `apn`. Returns
    /// the connected iface + ip on success.
    async fn bring_up(&self, apn: &str) -> Result<DbusConnectResult, String>;
    /// Disconnect the first modem's data session.
    async fn bring_down(&self) -> Result<(), String>;
    /// True if at least one modem object is present on the bus.
    async fn modem_present(&self) -> Result<bool, String>;
    /// The first modem's SIM IMSI, for carrier-APN auto-detection. `Ok(None)`
    /// when the bus has a modem but no readable IMSI; `Err` on any bus error.
    async fn imsi(&self) -> Result<Option<String>, String>;
    /// `Bearer.Connected` of the bearer at `bearer`.
    async fn bearer_connected(&self, bearer: &str) -> Result<bool, String>;
}

/// D-Bus path disabled (non-Linux dev host, or a build with no bus). Every call
/// errors so the manager immediately runs in fallback mode.
pub struct DisabledDbus;

#[async_trait]
impl ModemDbus for DisabledDbus {
    async fn bring_up(&self, _apn: &str) -> Result<DbusConnectResult, String> {
        Err("dbus_disabled".into())
    }
    async fn bring_down(&self) -> Result<(), String> {
        Err("dbus_disabled".into())
    }
    async fn modem_present(&self) -> Result<bool, String> {
        Err("dbus_disabled".into())
    }
    async fn imsi(&self) -> Result<Option<String>, String> {
        Err("dbus_disabled".into())
    }
    async fn bearer_connected(&self, _bearer: &str) -> Result<bool, String> {
        Err("dbus_disabled".into())
    }
}

/// The AT control-port seam. The production impl opens the first answering
/// serial port via `modem_at`; tests inject a fake that hands back a scripted
/// transport so the fallback bring-up is exercised without a real modem.
#[async_trait]
pub trait AtPortOpener: Send + Sync {
    /// Open and AT-probe a control port. `None` when no port answers (no modem
    /// present, or a non-Linux host).
    async fn open(&self) -> Option<Box<dyn SerialTransport>>;
}

/// Production AT opener: scans `/dev` for a serial control port that answers AT.
pub struct RealAtPortOpener;

#[async_trait]
impl AtPortOpener for RealAtPortOpener {
    async fn open(&self) -> Option<Box<dyn SerialTransport>> {
        modem_at::open_control_port().await
    }
}

/// Single-modem cellular data manager with D-Bus-first, AT-fallback. HW-gated
/// and disabled by default.
pub struct ModemManager {
    dbus: Arc<dyn ModemDbus>,
    at_opener: Arc<dyn AtPortOpener>,
    /// Runs `ip` for a static-IP bearer and the IPv4 liveness read.
    runner: Arc<dyn CmdRunner>,
    config_path: PathBuf,
    net_dir: PathBuf,
    /// The interface the connected bearer reported (`Bearer.Interface`).
    /// A std mutex: read from the sync uplink-manager accessors, never held
    /// across an await.
    bearer_iface: std::sync::Mutex<Option<String>>,
    /// The provisioned-gadget marker dir. When it exists, `usb0` is the USB
    /// tether NIC the daemon created, not the modem, so the modem skips it.
    gadget_dir: PathBuf,
    state: Mutex<ModemState>,
    /// Serializes bring-up and tear-down against each other. Held across the
    /// slow D-Bus / AT dial so the state mutex never is, keeping `status`,
    /// `enabled`, `configure` and `reload_config` responsive during a dial.
    dial: Mutex<()>,
}

#[derive(Default)]
struct ModemState {
    dbus_fail_count: u32,
    fallback_mode: bool,
    /// When the last D-Bus retry from fallback was made, so fallback probes
    /// D-Bus again on a fixed cadence instead of never.
    last_dbus_retry: Option<Instant>,
    config: ModemConfig,
    brought_up: bool,
    /// The bearer object path of the D-Bus session, for `Bearer.Connected`.
    bearer: Option<String>,
    /// The AT control port held open across an AT-fallback bring-up so status
    /// polls reuse it. `None` until a fallback bring-up succeeds. Not `Debug`,
    /// so this struct does not derive `Debug`.
    at_port: Option<Box<dyn SerialTransport>>,
}

impl ModemManager {
    /// Manager with the production zbus D-Bus client (Linux) or the disabled
    /// client (off Linux), reading the canonical config + sysfs paths.
    pub fn new() -> Self {
        Self::with_parts(
            default_dbus(),
            PathBuf::from(crate::paths::GS_MODEM_JSON),
            PathBuf::from("/sys/class/net"),
        )
    }

    /// Constructor with the real AT opener (tests inject a fake D-Bus client +
    /// tempdir paths but exercise the production serial scan, which no-ops off a
    /// modem).
    pub fn with_parts(dbus: Arc<dyn ModemDbus>, config_path: PathBuf, net_dir: PathBuf) -> Self {
        Self::with_parts_at(dbus, Arc::new(RealAtPortOpener), config_path, net_dir)
    }

    /// Full constructor (tests inject a fake D-Bus client + a fake AT opener +
    /// tempdir paths) so the AT-fallback bring-up is unit-testable.
    pub fn with_parts_at(
        dbus: Arc<dyn ModemDbus>,
        at_opener: Arc<dyn AtPortOpener>,
        config_path: PathBuf,
        net_dir: PathBuf,
    ) -> Self {
        let config = load_config(&config_path);
        Self {
            dbus,
            at_opener,
            runner: Arc::new(TokioCmdRunner),
            config_path,
            net_dir,
            bearer_iface: std::sync::Mutex::new(None),
            gadget_dir: PathBuf::from(USB_GADGET_DIR),
            state: Mutex::new(ModemState {
                config,
                ..Default::default()
            }),
            dial: Mutex::new(()),
        }
    }

    /// Override the provisioned-gadget marker dir (tests point it at a tempdir
    /// to exercise the `usb0`-is-the-tether carve-out without touching configfs).
    pub fn with_gadget_dir(mut self, gadget_dir: PathBuf) -> Self {
        self.gadget_dir = gadget_dir;
        self
    }

    /// Override the command runner (tests script the `ip` calls).
    pub fn with_runner(mut self, runner: Arc<dyn CmdRunner>) -> Self {
        self.runner = runner;
        self
    }
}

#[async_trait]
impl UplinkManager for ModemManager {
    async fn is_up(&self) -> bool {
        // Liveness only: the cellular link is "up" when the kernel iface is up.
        // Bringing the modem up is an explicit, config-gated action, never a
        // side effect of a probe.
        self.iface_up()
    }
    fn get_iface(&self) -> String {
        self.current_iface()
    }
    async fn get_gateway(&self) -> Option<String> {
        // The cellular gateway is point-to-point; the default route is set by
        // ModemManager / the bring-up. The router reads it via `ip route` for
        // the active uplink, so no manager-side gateway is reported here.
        None
    }
}

impl Default for ModemManager {
    fn default() -> Self {
        Self::new()
    }
}

fn read_counter(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn load_config(path: &Path) -> ModemConfig {
    ModemConfig::load(path)
}

fn config_to_json(cfg: &ModemConfig) -> Value {
    json!({
        "apn": cfg.apn,
        "cap_gb": cfg.cap_gb,
        "enabled": cfg.enabled,
    })
}

/// The production D-Bus client: zbus on Linux, disabled elsewhere.
#[cfg(target_os = "linux")]
fn default_dbus() -> Arc<dyn ModemDbus> {
    Arc::new(zbus_impl::ZbusModem::new())
}

#[cfg(not(target_os = "linux"))]
fn default_dbus() -> Arc<dyn ModemDbus> {
    Arc::new(DisabledDbus)
}

#[cfg(target_os = "linux")]
mod zbus_impl;

/// A cellular netdev under `net`, bound to `driver` through a USB device
/// node (tests rig sysfs with it so driver-and-bus resolution sees a modem).
#[cfg(all(test, unix))]
pub(crate) fn rig_cellular(net: &Path, name: &str, driver: &str) {
    let root = net.parent().unwrap_or(net);
    let dev = root.join("devices").join(format!("usb1/1-1/{name}"));
    let drv = root.join("drivers").join(driver);
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::create_dir_all(&drv).unwrap();
    let _ = std::os::unix::fs::symlink(&drv, dev.join("driver"));
    std::fs::create_dir_all(net.join(name)).unwrap();
    let _ = std::os::unix::fs::symlink(&dev, net.join(name).join("device"));
}

#[cfg(test)]
#[path = "modem_tests.rs"]
mod tests;
