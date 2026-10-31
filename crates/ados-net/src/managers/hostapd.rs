//! WiFi AP lifecycle for the ground-station profile.
//!
//! Runs `hostapd` on the onboard wlan0 so phones, tablets, and laptops join a
//! stable SSID (`ADOS-GS-<short_id>`) and reach the setup webapp, WHEP video,
//! and agent REST API. A matching `dnsmasq` serves DHCP on 192.168.4.0/24. The
//! RTL8812 USB adapter is reserved for monitor-mode WFB-ng RX elsewhere and is
//! never touched here. Solo-benchable: config rendering + passphrase resolution
//! need no radio; start/stop are systemctl calls through the injectable command
//! runner.
//!
//! `ados-hostapd.service` execs `/usr/sbin/hostapd` itself, so "is the unit
//! active?" only says the hostapd process is alive; it does not prove an SSID
//! is being broadcast.
//!
//! Because of that, unit state alone is not accepted as proof here. The radio
//! itself is asked (`iw dev <iface> info`: interface type plus whether the phy
//! holds an operating channel, which it only does once the BSS is beaconing),
//! and a transmit delta counter corroborates it. See [`ApLiveness`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::cmd::CmdRunner;

mod config;
mod health;
mod lifecycle;

/// 2.4 GHz channels a `hw_mode=g` AP may use (the regulatory set; channel 14
/// is 802.11b-only).
const AP_CHANNELS_G: std::ops::RangeInclusive<u32> = 1..=13;

/// Check AP settings before any of them reach `hostapd.conf`, the one place
/// they are interpolated raw. An SSID is 1-32 bytes with no control
/// characters; a WPA2 passphrase is 8-63 printable ASCII characters; the
/// channel is one `hw_mode=g` allows. A newline in either string would inject
/// a hostapd directive (`wpa=0` after `wpa=2` opens the AP), and an
/// out-of-range value would fail hostapd on restart and take the AP down.
/// `None` means "unchanged" and is not checked.
pub fn validate_ap_settings(
    ssid: Option<&str>,
    passphrase: Option<&str>,
    channel: Option<u32>,
) -> Result<(), String> {
    if let Some(s) = ssid {
        if s.is_empty() || s.len() > 32 || s.chars().any(char::is_control) {
            return Err("SSID must be 1-32 bytes with no control characters".to_string());
        }
    }
    if let Some(p) = passphrase {
        if !(8..=63).contains(&p.len()) || !p.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            return Err("passphrase must be 8-63 printable ASCII characters".to_string());
        }
    }
    if let Some(c) = channel {
        if !AP_CHANNELS_G.contains(&c) {
            return Err(format!(
                "channel {c} is not a 2.4 GHz channel ({}-{})",
                AP_CHANNELS_G.start(),
                AP_CHANNELS_G.end()
            ));
        }
    }
    Ok(())
}

const AP_ADDR: &str = "192.168.4.1";
const AP_CIDR: &str = "192.168.4.1/24";
const DHCP_RANGE: &str = "192.168.4.10,192.168.4.100,12h";
const HOSTAPD_UNIT: &str = "ados-hostapd.service";
const DNSMASQ_UNIT: &str = "ados-dnsmasq-gs.service";

const CMD_TIMEOUT: Duration = Duration::from_secs(10);
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the per-interface kernel counters live. A field on the manager so the
/// delta probe is exercisable off a real radio.
const SYSFS_NET_ROOT: &str = "/sys/class/net";

/// How long the AP interface's transmit counter may stay flat, *while at least
/// one station is associated*, before hostapd is judged wedged.
///
/// The station qualifier is load-bearing and not conservatism for its own sake:
/// on mac80211 beacons are emitted by the driver's beacon path and are not
/// accounted into the netdev `tx_packets` counter, so an idle AP with no client
/// legitimately shows a flat counter forever. Condemning on the counter alone
/// would restart a perfectly healthy AP every window. An associated station
/// that is receiving nothing is the case where flatness does mean a wedge.
const TX_STALL_WINDOW: Duration = Duration::from_secs(30);

/// How long to wait for the phy to enter AP mode with an operating channel
/// after `systemctl start`, and how often to re-ask inside that budget.
/// `Type=simple` means systemctl returns as soon as the fork succeeds, so the
/// exit code says nothing about whether the BSS came up.
const AP_SETTLE_BUDGET: Duration = Duration::from_secs(6);
const AP_SETTLE_POLL: Duration = Duration::from_millis(500);

/// What the radio says about the AP interface. Every field is absent rather
/// than defaulted when it could not be read: "the probe did not answer" and
/// "the probe answered, and the answer is no" are different facts and a status
/// surface that merges them reports a guess as a measurement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadioView {
    /// The `iw dev <iface> info` probe produced a parseable answer at all.
    pub probe_ok: bool,
    /// `AP`, `managed`, `monitor`, … as the kernel reports it.
    pub iface_type: Option<String>,
    /// The channel the phy currently holds. Present only once the BSS is up,
    /// which makes it the cheapest honest "the SSID is on the air" signal.
    pub operating_channel: Option<u32>,
    /// The SSID the kernel reports for the interface (not the configured one).
    pub ssid: Option<String>,
}

/// Verdict of one liveness pass over a unit that is already `active`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApLiveness {
    /// The phy is in AP mode with an operating channel: the SSID is on the air.
    Radiating,
    /// hostapd is alive and the AP is not being served. Carries a stable reason
    /// string for the log and the sidecar.
    Stalled(&'static str),
    /// No usable signal. NEVER acted on: a probe that could not be made is not
    /// evidence of a fault, and restarting the AP on it would turn a missing
    /// `iw` binary into a reboot loop of the operator's only reachability.
    Unknown(&'static str),
    /// The hostapd unit is not active: no AP at all. Never acted on here; the
    /// setup-AP guard owns starting it (and retries every tick).
    Down,
}

/// Last observed transmit counter on the AP interface and when it last moved.
#[derive(Debug, Clone, Copy)]
struct TxSample {
    packets: u64,
    flat_since: Instant,
}

/// Parse `iw dev <iface> info`. Pure, so the shape of every kernel answer this
/// has to survive is pinned by tests rather than by a live radio.
///
/// `probe_ok` requires a `type` line: every `iw` version prints one, so its
/// absence means the interface does not exist, the binary is missing, or the
/// call timed out — none of which is evidence about the AP.
pub fn parse_iw_info(stdout: &str) -> RadioView {
    let mut view = RadioView::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("type ") {
            view.iface_type = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("channel ") {
            view.operating_channel = rest.split_whitespace().next().and_then(|c| c.parse().ok());
        } else if let Some(rest) = line.strip_prefix("ssid ") {
            view.ssid = Some(rest.trim().to_string());
        }
    }
    view.probe_ok = view.iface_type.is_some();
    view
}

/// First four hex chars of `device_id`, uppercased; zero-padded when there are
/// fewer than four after stripping non-hex characters.
pub fn short_id(device_id: &str) -> String {
    let hex_only: String = device_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    let padded = if hex_only.len() >= 4 {
        hex_only
    } else {
        format!("{hex_only}0000")
    };
    padded[..4].to_uppercase()
}

/// The AP SSID for a device id (`ADOS-GS-<short_id>`).
pub fn build_ssid(device_id: &str) -> String {
    format!("ADOS-GS-{}", short_id(device_id))
}

/// Manages hostapd + dnsmasq for the ground-station AP. One per agent;
/// idempotent.
pub struct HostapdManager {
    ssid: String,
    channel: u32,
    interface: String,
    configured_passphrase: String,
    passphrase: String,
    /// The country hostapd advertises.
    ///
    /// Was a hardcoded `IN` while the radio's own reconciler defaulted to
    /// `US`, so a stock box ran its access point and its radio under two
    /// different declared jurisdictions. Resolved from the same operator
    /// setting the radio reads, so the two agree.
    country_code: String,
    hostapd_conf_path: PathBuf,
    dnsmasq_conf_path: PathBuf,
    passphrase_path: PathBuf,
    runner: Arc<dyn CmdRunner>,
    /// Root of the per-interface kernel counter tree (`/sys/class/net`).
    /// Overridable so the transmit delta probe runs without a radio.
    sysfs_net_root: PathBuf,
    /// Last transmit counter seen on the AP interface and when it last moved.
    /// Interior mutability because the liveness probe runs off `&self` (the
    /// status and supervise paths hold a shared borrow behind the daemon's
    /// `Arc<Mutex<..>>`).
    tx_sample: Mutex<Option<TxSample>>,
}

impl HostapdManager {
    /// Manager with canonical paths. `ssid` defaults to `ADOS-GS-<short_id>`
    /// when `None`; channel defaults to 6 elsewhere (pass it explicitly).
    pub fn new(
        device_id: &str,
        ssid: Option<String>,
        channel: u32,
        configured_passphrase: String,
        runner: Arc<dyn CmdRunner>,
    ) -> Self {
        let mut m = Self::with_paths(
            device_id,
            ssid,
            channel,
            configured_passphrase,
            runner,
            PathBuf::from(crate::paths::HOSTAPD_CONF_PATH),
            PathBuf::from(crate::paths::DNSMASQ_CONF_PATH),
            PathBuf::from(crate::paths::AP_PASSPHRASE_PATH),
        );
        m.resolve_interface(&ados_protocol::ap_country::configured_ap_interface());
        m
    }

    /// Resolve which radio the access point runs on, replacing the `wlan0`
    /// guess.
    ///
    /// Interface names are not stable: on this bench `wlan0` was the onboard
    /// chip on two boots out of three and the USB flight radio on the third.
    /// Binding the AP to a name therefore meant a one-in-three chance of
    /// configuring hostapd on the aircraft's radio link.
    ///
    /// Resolution is by DRIVER, cross-checked against the interface the radio
    /// says it actually took. A failure leaves the interface unresolved (empty)
    /// and is logged loudly; `write_config` then refuses, so the AP is never
    /// configured on a guessed name.
    pub fn resolve_interface(&mut self, configured: &str) {
        let radio = ados_protocol::netif::radio_interface();
        match ados_protocol::netif::resolve_ap_interface(configured, radio.as_deref()) {
            Ok(iface) => {
                if iface != self.interface {
                    info!(
                        from = %self.interface, to = %iface, radio = ?radio,
                        "ap_interface_resolved"
                    );
                }
                self.interface = iface;
            }
            Err(e) => {
                error!(error = %e, radio = ?radio, "ap_interface_unresolved");
                self.interface.clear();
            }
        }
    }

    /// Full constructor (tests).
    #[allow(clippy::too_many_arguments)]
    pub fn with_paths(
        device_id: &str,
        ssid: Option<String>,
        channel: u32,
        configured_passphrase: String,
        runner: Arc<dyn CmdRunner>,
        hostapd_conf_path: PathBuf,
        dnsmasq_conf_path: PathBuf,
        passphrase_path: PathBuf,
    ) -> Self {
        Self {
            ssid: ssid.unwrap_or_else(|| build_ssid(device_id)),
            channel,
            interface: String::new(),
            configured_passphrase,
            passphrase: String::new(),
            country_code: ados_protocol::ap_country::load(),
            hostapd_conf_path,
            dnsmasq_conf_path,
            passphrase_path,
            runner,
            sysfs_net_root: PathBuf::from(SYSFS_NET_ROOT),
            tx_sample: Mutex::new(None),
        }
    }

    /// Point the transmit delta probe at a different counter tree. Tests only:
    /// production always reads `/sys/class/net`.
    pub fn set_sysfs_net_root(&mut self, root: PathBuf) {
        self.sysfs_net_root = root;
    }

    /// Pin the AP interface as if [`Self::resolve_interface`] had resolved it.
    /// Tests only: production always resolves by driver.
    #[cfg(test)]
    pub(crate) fn set_interface(&mut self, iface: &str) {
        self.interface = iface.to_string();
    }

    pub fn ssid(&self) -> &str {
        &self.ssid
    }
    pub fn channel(&self) -> u32 {
        self.channel
    }
    pub fn interface(&self) -> &str {
        &self.interface
    }
    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }
}

#[cfg(test)]
mod tests;
