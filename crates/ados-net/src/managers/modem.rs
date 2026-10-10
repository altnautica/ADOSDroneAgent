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

    /// True when the manager has flipped to AT fallback (the Python AT service
    /// owns the link in this state — see the module-level seam note).
    pub async fn needs_at_fallback(&self) -> bool {
        self.state.lock().await.fallback_mode
    }

    /// Whether the operator enabled the modem in the sidecar. The daemon gates
    /// auto bring-up on this; default (absent key) is treated as enabled to
    /// match the Python `config.get("enabled", True)`, BUT the daemon only
    /// brings the modem up at all when a config file exists.
    pub async fn enabled(&self) -> bool {
        self.state.lock().await.config.enabled.unwrap_or(true)
    }

    fn register_dbus_failure(&self, st: &mut ModemState, reason: &str) {
        st.dbus_fail_count += 1;
        warn!(
            count = st.dbus_fail_count,
            reason = reason,
            "modem.dbus_fail"
        );
        if st.dbus_fail_count >= DBUS_FAIL_THRESHOLD && !st.fallback_mode {
            st.fallback_mode = true;
            warn!("modem.fallback_to_at");
        }
    }

    fn register_dbus_success(&self, st: &mut ModemState) {
        if st.dbus_fail_count > 0 {
            st.dbus_fail_count = 0;
        }
        if st.fallback_mode {
            st.fallback_mode = false;
            info!("modem.dbus_recovered");
        }
    }

    /// Bring up the cellular data session. D-Bus first; on failure the manager
    /// advances its failure counter and (past threshold) flips to the AT
    /// fallback. In fallback D-Bus is retried every [`DBUS_RETRY_INTERVAL`], so
    /// a transient D-Bus outage does not pin the modem to AT for good. Returns
    /// a status dict. `apn = "auto"` resolves via the supplied IMSI ("auto"
    /// with no IMSI falls back to `internet`).
    ///
    /// The state mutex is held only to snapshot and to record results, never
    /// across the dial itself.
    pub async fn bring_up(&self, apn: &str, imsi: Option<&str>) -> Value {
        let _dial = self.dial.lock().await;
        let resolved = if apn == "auto" {
            imsi.and_then(apn_for_imsi)
                .unwrap_or(DEFAULT_APN_FALLBACK)
                .to_string()
        } else {
            apn.to_string()
        };

        let try_dbus = {
            let mut st = self.state.lock().await;
            let due = !st.fallback_mode
                || st
                    .last_dbus_retry
                    .is_none_or(|t| t.elapsed() >= DBUS_RETRY_INTERVAL);
            if st.fallback_mode && due {
                st.last_dbus_retry = Some(Instant::now());
            }
            due
        };
        if try_dbus {
            let res = self.dbus.bring_up(&resolved).await;
            match res {
                Ok(res) => {
                    {
                        let mut st = self.state.lock().await;
                        self.register_dbus_success(&mut st);
                        st.brought_up = true;
                        st.at_port = None;
                        st.bearer = (!res.bearer.is_empty()).then(|| res.bearer.clone());
                    }
                    let reported = (!res.iface.is_empty()).then(|| res.iface.clone());
                    self.set_bearer_iface(reported.clone());
                    let iface = self.cellular_iface().or(reported).unwrap_or_default();
                    let mut ip = res.ip.clone();
                    if let (Some(cfg), false) = (&res.ip4_static, iface.is_empty()) {
                        self.apply_static_ip4(&iface, cfg).await;
                        ip = cfg.address.clone();
                    }
                    return json!({
                        "connected": true,
                        "iface": iface,
                        "ip": ip,
                        "apn": res.apn,
                        "fallback_mode": false,
                    });
                }
                Err(reason) => {
                    let mut st = self.state.lock().await;
                    self.register_dbus_failure(&mut st, &reason);
                }
            }
        }

        // Fallback: D-Bus is unavailable. Drive the AT control port directly.
        // `apn` is passed through as "auto" so the AT driver reads the SIM IMSI
        // (AT+CIMI) and maps the carrier APN itself, matching the D-Bus path's
        // auto behaviour. On a board with no modem the port opener returns None
        // and the manager reports it still needs AT (nothing to drive).
        let net_dir = self.net_dir.clone();
        let iface_present = move |iface: &str| net_dir.join(iface).exists();
        match self.at_opener.open().await {
            Some(mut port) => {
                let result = modem_at::bring_up_over(port.as_mut(), apn, iface_present).await;
                let connected = result
                    .get("connected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut st = self.state.lock().await;
                st.brought_up = connected;
                // Cache the open port so status polls reuse it.
                st.at_port = connected.then_some(port);
                result
            }
            None => {
                self.state.lock().await.brought_up = false;
                json!({
                    "connected": false,
                    "iface": USB_IFACE,
                    "ip": "",
                    "apn": resolved,
                    "fallback_mode": true,
                    "needs_at_fallback": true,
                })
            }
        }
    }

    /// Whether the data session the daemon dialed is still in place. A D-Bus
    /// session answers from its bearer's `Connected` property, so a carrier
    /// detach that leaves the netdev up still reads down and is re-dialed.
    /// Without a readable bearer: the modem netdev is present, not down
    /// (raw-IP WWAN netdevs report `unknown` while carrying traffic) and holds
    /// an IPv4 address.
    pub async fn session_up(&self) -> bool {
        let bearer = {
            let st = self.state.lock().await;
            if !st.brought_up {
                return false;
            }
            st.bearer.clone()
        };
        if let Some(path) = bearer {
            if let Ok(connected) = self.dbus.bearer_connected(&path).await {
                return connected;
            }
        }
        let Some(iface) = self.cellular_iface() else {
            return false;
        };
        let oper = std::fs::read_to_string(self.net_dir.join(&iface).join("operstate"))
            .is_ok_and(|s| matches!(s.trim(), "up" | "unknown"));
        if !oper {
            return false;
        }
        let out = self
            .runner
            .run(&["ip", "-4", "addr", "show", "dev", &iface], IP_TIMEOUT)
            .await;
        out.ok() && crate::managers::ethernet::parse_inet(&out.stdout).is_some()
    }

    /// Apply a static-method bearer's IPv4 settings: the address, then a
    /// default route at [`STATIC_ROUTE_METRIC`] when a gateway was given.
    async fn apply_static_ip4(&self, iface: &str, cfg: &Ip4Static) {
        let cidr = format!("{}/{}", cfg.address, cfg.prefix);
        let addr = self
            .runner
            .run(&["ip", "addr", "replace", &cidr, "dev", iface], IP_TIMEOUT)
            .await;
        if !addr.ok() {
            warn!(iface, rc = addr.rc, stderr = %addr.stderr.trim(), "modem.static_addr_failed");
        }
        if let Some(gw) = &cfg.gateway {
            let route = self
                .runner
                .run(
                    &[
                        "ip",
                        "route",
                        "replace",
                        "default",
                        "via",
                        gw,
                        "dev",
                        iface,
                        "metric",
                        STATIC_ROUTE_METRIC,
                    ],
                    IP_TIMEOUT,
                )
                .await;
            if !route.ok() {
                warn!(iface, rc = route.rc, stderr = %route.stderr.trim(), "modem.static_route_failed");
            }
        }
        info!(iface, address = %cidr, gateway = ?cfg.gateway, "modem.static_ip4_applied");
    }

    fn set_bearer_iface(&self, iface: Option<String>) {
        *self.bearer_iface.lock().unwrap_or_else(|e| e.into_inner()) = iface;
    }

    /// Tear the data session down (best-effort D-Bus, then a raw link-down via the
    /// iface operstate is left to the daemon).
    pub async fn bring_down(&self) -> Value {
        let _dial = self.dial.lock().await;
        let fallback_mode = self.state.lock().await.fallback_mode;
        let mut ok = false;
        if !fallback_mode {
            let res = self.dbus.bring_down().await;
            let mut st = self.state.lock().await;
            match res {
                Ok(()) => {
                    self.register_dbus_success(&mut st);
                    ok = true;
                }
                Err(reason) => self.register_dbus_failure(&mut st, &reason),
            }
        }
        let mut st = self.state.lock().await;
        st.brought_up = false;
        st.bearer = None;
        // Drop the cached AT port so the next bring-up re-opens a clean port.
        st.at_port = None;
        drop(st);
        self.set_bearer_iface(None);
        json!({ "ok": ok })
    }

    /// Daemon-side modem status: liveness + mode + (in AT fallback) the
    /// AT-polled signal / technology / operator / imei. The D-Bus path exposes
    /// these properties through ModemManager and the in-process Python manager
    /// reads them via mmcli, so on a D-Bus board the daemon does not duplicate
    /// that read; it reports `mode: dbus` and leaves the rich fields to the REST
    /// layer. In AT fallback the daemon owns the only serial handle, so it polls
    /// AT here. Always reports iface liveness from sysfs.
    pub async fn status(&self) -> Value {
        // Snapshot the fast state fields and TAKE the held-open AT port out of
        // the locked state, then DROP the guard before any slow I/O. Holding the
        // async state mutex across the serial AT round-trip (and the D-Bus
        // round-trip) would serialize enabled()/bring_up()/bring_down()/
        // configure() behind a slow or hung poll. With the port taken out, those
        // fast methods stay responsive while this poll runs lock-free; the port
        // is re-stored under a brief re-lock at the end.
        let (fallback_mode, brought_up, mut at_port) = {
            let mut st = self.state.lock().await;
            (
                st.fallback_mode,
                st.brought_up,
                std::mem::take(&mut st.at_port),
            )
        };

        // Lock released. The D-Bus liveness probe runs without the state lock.
        let present = self.dbus.modem_present().await.unwrap_or(false);
        let iface = self.current_iface();
        let up = self.iface_up();
        let mode = if fallback_mode { "at" } else { "dbus" };

        let mut out = json!({
            "mode": mode,
            "modem_present": present || fallback_mode,
            "iface": iface,
            "iface_up": up,
            "brought_up": brought_up,
            "needs_at_fallback": fallback_mode,
        });

        // In AT fallback, poll the taken-out port for the rich fields. Still
        // lock-free here, so a slow/hung serial exchange never blocks the other
        // manager methods.
        if fallback_mode {
            if let Some(port) = at_port.as_mut() {
                let at = modem_at::status_over(port.as_mut()).await;
                if let (Value::Object(dst), Value::Object(src)) = (&mut out, at) {
                    dst.extend(src);
                }
            }
        }

        // Re-store the port so the next poll reuses it. If a concurrent
        // bring_down() ran while we held the port out, it set at_port = None;
        // do not clobber that teardown with our stale handle. If a concurrent
        // bring_up() re-opened a fresh port, prefer the fresh one and drop ours.
        if let Some(port) = at_port {
            let mut st = self.state.lock().await;
            if st.at_port.is_none() && st.brought_up && st.fallback_mode {
                st.at_port = Some(port);
            }
            // else: teardown or a fresh re-open won the race; our handle is
            // stale, so let it drop (closing the serial fd cleanly).
        }
        out
    }

    /// True if a modem is reachable: a present D-Bus modem, or (in fallback) an
    /// up `usb0`/`wwan0` iface. A cheap liveness probe for the daemon's health
    /// loop that never auto-connects.
    pub async fn probe(&self) -> bool {
        if self.dbus.modem_present().await.unwrap_or(false) {
            return true;
        }
        self.iface_up()
    }

    /// Read byte counters from `/sys/class/net/<iface>/statistics`. Pure sysfs; returns
    /// zeros + `available:false` when the iface is absent. (The chunk-2 data-cap
    /// tracker reads the same counters via its own sysfs source; this is the
    /// modem-scoped view for the API.)
    pub fn data_usage(&self) -> Value {
        let iface = self.current_iface();
        let base = self.net_dir.join(&iface).join("statistics");
        let rx = read_counter(&base.join("rx_bytes"));
        let tx = read_counter(&base.join("tx_bytes"));
        match (rx, tx) {
            (Some(rx), Some(tx)) => json!({
                "rx_bytes": rx,
                "tx_bytes": tx,
                "total_bytes": rx + tx,
                "iface": iface,
                "last_read": now_secs(),
                "available": true,
            }),
            _ => json!({
                "rx_bytes": 0,
                "tx_bytes": 0,
                "total_bytes": 0,
                "iface": iface,
                "last_read": now_secs(),
                "available": false,
            }),
        }
    }

    /// Update the persisted config sidecar (atomic, byte-parity write). Returns
    /// the new config as a dict, or an error when the APN is not a valid APN
    /// (nothing is written then). The bring-up/down side effect is driven by
    /// the daemon, not here, to keep this lock-free of I/O on the link.
    pub async fn configure(
        &self,
        apn: Option<&str>,
        cap_gb: Option<f64>,
        enabled: Option<bool>,
    ) -> Result<Value, String> {
        if let Some(a) = apn {
            validate_apn(a)?;
        }
        let mut st = self.state.lock().await;
        let mut cfg = st.config.clone();
        let mut changed = false;
        if let Some(a) = apn {
            if cfg.apn.as_deref() != Some(a) {
                cfg.apn = Some(a.to_string());
                changed = true;
            }
        }
        if let Some(c) = cap_gb {
            if cfg.cap_gb != Some(c) {
                cfg.cap_gb = Some(c);
                changed = true;
            }
        }
        if let Some(e) = enabled {
            if cfg.enabled != Some(e) {
                cfg.enabled = Some(e);
                changed = true;
            }
        }
        if changed {
            if let Err(exc) = sidecar::write_atomic(&self.config_path, cfg.render_json().as_bytes())
            {
                warn!(error = %exc, "modem.config_write_failed");
            } else {
                info!("modem.config_updated");
                st.config = cfg.clone();
            }
        }
        Ok(config_to_json(&st.config))
    }

    /// Re-read the persisted config sidecar into the manager's live config, so
    /// `enabled()` and the dialed APN reflect a file the REST modem write path
    /// rewrote out-of-process. Returns the freshly-loaded config. The daemon
    /// calls this each poll before reconciling the session, the same
    /// per-tick-reread contract the data-cap tracker uses for the cellular cap.
    pub async fn reload_config(&self) -> ModemConfig {
        let cfg = load_config(&self.config_path);
        let mut st = self.state.lock().await;
        st.config = cfg.clone();
        cfg
    }

    /// The configured APN for a dial, or `"auto"` when none is set (so carrier
    /// detection runs). Reads the live config.
    pub async fn configured_apn(&self) -> String {
        self.state
            .lock()
            .await
            .config
            .apn
            .clone()
            .unwrap_or_else(|| "auto".to_string())
    }

    /// Read the live SIM IMSI for carrier-APN auto-detection. Prefers the D-Bus
    /// 3GPP property; if the bus has none (D-Bus absent or no SIM) returns
    /// `None` and the AT fallback reads `AT+CIMI` itself during bring-up. Used
    /// by the daemon to pass a resolved IMSI to [`bring_up`] so `apn_for_imsi`
    /// works on the D-Bus path.
    ///
    /// [`bring_up`]: ModemManager::bring_up
    pub async fn read_imsi(&self) -> Option<String> {
        // Err (bus failure) and Ok(None) both mean "no IMSI to pass through".
        self.dbus.imsi().await.unwrap_or_default()
    }

    /// The active cellular iface, empty when there is none (so `is_up` and
    /// `iface_up` read false).
    fn current_iface(&self) -> String {
        self.cellular_iface().unwrap_or_default()
    }

    /// The resolved cellular iface, but ONLY when it is actually present in
    /// sysfs (and not the USB-gadget tether) — `None` otherwise. This is the
    /// metering iface: the data-cap source counts bytes on it, so the data-cap
    /// throttle must shape THIS iface, not whatever the router currently routes
    /// through. Returning `None` when no modem iface exists lets the throttle
    /// consumer no-op rather than shape a wired primary by mistake.
    ///
    /// The bearer's own `Interface` wins while it is present; otherwise the
    /// interface is found by driver and bus ([`resolve_cellular_iface`]).
    pub fn cellular_iface(&self) -> Option<String> {
        let reported = self
            .bearer_iface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(iface) = reported {
            if self.net_dir.join(&iface).exists() {
                return Some(iface);
            }
        }
        resolve_cellular_iface(&self.net_dir, &self.gadget_dir)
    }

    /// Iface operstate == "up" (HW-gated liveness; reading sysfs never
    /// auto-connects the modem).
    fn iface_up(&self) -> bool {
        let Some(iface) = self.cellular_iface() else {
            return false;
        };
        std::fs::read_to_string(self.net_dir.join(&iface).join("operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false)
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

/// zbus-backed ModemManager1 client. Pure-Rust D-Bus (no dbus-sys / libdbus,
/// no ring); reuses the tokio runtime via zbus's `tokio` feature.
#[cfg(target_os = "linux")]
mod zbus_impl {
    use super::*;
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};
    use zbus::{Connection, Proxy};

    /// `MM_BEARER_IP_METHOD_STATIC`: the host must apply the address itself.
    const IP_METHOD_STATIC: u32 = 2;

    /// The static IPv4 settings out of a bearer's `Ip4Config` dict, or `None`
    /// when the method is not static or the address is missing.
    fn parse_ip4_static(cfg: &HashMap<String, OwnedValue>) -> Option<Ip4Static> {
        let method = cfg.get("method").and_then(|v| u32::try_from(v).ok())?;
        if method != IP_METHOD_STATIC {
            return None;
        }
        let address = cfg
            .get("address")
            .and_then(|v| <&str>::try_from(v).ok())
            .filter(|a| !a.is_empty())?
            .to_string();
        let prefix = cfg
            .get("prefix")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(32);
        let gateway = cfg
            .get("gateway")
            .and_then(|v| <&str>::try_from(v).ok())
            .filter(|g| !g.is_empty())
            .map(str::to_string);
        Some(Ip4Static {
            address,
            prefix,
            gateway,
        })
    }

    /// The ObjectManager `GetManagedObjects` reply shape (`a{oa{sa{sv}}}`); we
    /// only need the object-path keys.
    type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

    pub struct ZbusModem;

    impl ZbusModem {
        pub fn new() -> Self {
            Self
        }

        async fn connect(&self) -> Result<Connection, String> {
            tokio::time::timeout(DBUS_TIMEOUT, Connection::system())
                .await
                .map_err(|_| "dbus_connect_timeout".to_string())?
                .map_err(|e| e.to_string())
        }

        async fn bearer_proxy<'a>(
            &self,
            conn: &'a Connection,
            bearer: &OwnedObjectPath,
        ) -> Result<Proxy<'a>, String> {
            Proxy::new(
                conn,
                MM_SERVICE,
                bearer.as_str().to_string(),
                "org.freedesktop.ModemManager1.Bearer",
            )
            .await
            .map_err(|e| e.to_string())
        }

        /// First `/Modem/N` object path (skipping `/Bearer/` children).
        async fn first_modem_path(&self, conn: &Connection) -> Result<String, String> {
            let om = Proxy::new(
                conn,
                MM_SERVICE,
                MM_ROOT_PATH,
                "org.freedesktop.DBus.ObjectManager",
            )
            .await
            .map_err(|e| e.to_string())?;

            let managed: ManagedObjects =
                tokio::time::timeout(DBUS_TIMEOUT, om.call("GetManagedObjects", &()))
                    .await
                    .map_err(|_| "dbus_list_timeout".to_string())?
                    .map_err(|e| e.to_string())?;

            let mut paths: Vec<String> = managed
                .keys()
                .map(|p| p.as_str().to_string())
                .filter(|p| p.contains("/ModemManager1/Modem/") && !p.contains("/Bearer/"))
                .collect();
            paths.sort();
            paths
                .into_iter()
                .next()
                .ok_or_else(|| "no_modem".to_string())
        }
    }

    #[async_trait]
    impl ModemDbus for ZbusModem {
        async fn bring_up(&self, apn: &str) -> Result<DbusConnectResult, String> {
            let conn = self.connect().await?;
            let path = self.first_modem_path(&conn).await?;

            // Enable is best-effort (some modems auto-enable); a failure here is
            // logged but not fatal, matching the Python `enable_skipped` path.
            if let Ok(modem) = Proxy::new(
                &conn,
                MM_SERVICE,
                path.clone(),
                "org.freedesktop.ModemManager1.Modem",
            )
            .await
            {
                let _ = tokio::time::timeout(DBUS_TIMEOUT, modem.call::<_, _, ()>("Enable", &true))
                    .await;
            }

            let simple = Proxy::new(
                &conn,
                MM_SERVICE,
                path,
                "org.freedesktop.ModemManager1.Modem.Simple",
            )
            .await
            .map_err(|e| e.to_string())?;

            let mut props: HashMap<&str, ZValue> = HashMap::new();
            props.insert("apn", ZValue::from(apn));
            // Connect returns the new bearer's object path: kept so the
            // manager can read the bearer's interface, IPv4 settings and
            // connection state.
            let bearer: OwnedObjectPath =
                tokio::time::timeout(CONNECT_TIMEOUT, simple.call("Connect", &props))
                    .await
                    .map_err(|_| "dbus_connect_timeout".to_string())?
                    .map_err(|e| e.to_string())?;

            let proxy = self.bearer_proxy(&conn, &bearer).await?;
            let iface =
                tokio::time::timeout(DBUS_TIMEOUT, proxy.get_property::<String>("Interface"))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
            let ip4 = tokio::time::timeout(
                DBUS_TIMEOUT,
                proxy.get_property::<HashMap<String, OwnedValue>>("Ip4Config"),
            )
            .await
            .ok()
            .and_then(Result::ok);
            let ip4_static = ip4.as_ref().and_then(parse_ip4_static);
            Ok(DbusConnectResult {
                iface,
                ip: ip4_static
                    .as_ref()
                    .map(|c| c.address.clone())
                    .unwrap_or_default(),
                apn: apn.to_string(),
                bearer: bearer.as_str().to_string(),
                ip4_static,
            })
        }

        async fn bearer_connected(&self, bearer: &str) -> Result<bool, String> {
            let conn = self.connect().await?;
            let path = OwnedObjectPath::try_from(bearer).map_err(|e| e.to_string())?;
            let proxy = self.bearer_proxy(&conn, &path).await?;
            tokio::time::timeout(DBUS_TIMEOUT, proxy.get_property::<bool>("Connected"))
                .await
                .map_err(|_| "dbus_bearer_timeout".to_string())?
                .map_err(|e| e.to_string())
        }

        async fn bring_down(&self) -> Result<(), String> {
            let conn = self.connect().await?;
            let path = self.first_modem_path(&conn).await?;
            let simple = Proxy::new(
                &conn,
                MM_SERVICE,
                path,
                "org.freedesktop.ModemManager1.Modem.Simple",
            )
            .await
            .map_err(|e| e.to_string())?;
            // Disconnect("/") tears down all bearers.
            let bearer = OwnedObjectPath::try_from("/").map_err(|e| e.to_string())?;
            tokio::time::timeout(
                DBUS_TIMEOUT * 2,
                simple.call::<_, _, ()>("Disconnect", &bearer),
            )
            .await
            .map_err(|_| "dbus_disconnect_timeout".to_string())?
            .map_err(|e| e.to_string())
        }

        async fn modem_present(&self) -> Result<bool, String> {
            let conn = self.connect().await?;
            match self.first_modem_path(&conn).await {
                Ok(_) => Ok(true),
                Err(e) if e == "no_modem" => Ok(false),
                Err(e) => Err(e),
            }
        }

        async fn imsi(&self) -> Result<Option<String>, String> {
            let conn = self.connect().await?;
            let path = match self.first_modem_path(&conn).await {
                Ok(p) => p,
                Err(e) if e == "no_modem" => return Ok(None),
                Err(e) => return Err(e),
            };
            // The SIM IMSI lives on the 3GPP interface's `Imsi` property.
            let modem3gpp = Proxy::new(
                &conn,
                MM_SERVICE,
                path,
                "org.freedesktop.ModemManager1.Modem.Modem3gpp",
            )
            .await
            .map_err(|e| e.to_string())?;
            match tokio::time::timeout(DBUS_TIMEOUT, modem3gpp.get_property::<String>("Imsi")).await
            {
                Ok(Ok(imsi)) if !imsi.is_empty() => Ok(Some(imsi)),
                // Property absent / empty (no SIM) → no IMSI, not an error.
                Ok(_) => Ok(None),
                Err(_) => Err("dbus_imsi_timeout".to_string()),
            }
        }
    }
}

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
