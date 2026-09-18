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
//! `ados-hostapd.service` execs `/usr/sbin/hostapd` itself. It used to exec the
//! Python AP manager, whose own `_HOSTAPD_UNIT` named that same unit, so
//! "start hostapd" asked systemd to start the process doing the asking and
//! "is hostapd active?" was answered by "is the asker alive?" — always true.
//! hostapd was consequently never executed and no SSID was ever broadcast,
//! while this manager, the REST status routes and dnsmasq's `Requires=` all
//! read the AP as up.
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

const AP_IFACE: &str = "wlan0";
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
/// fewer than four after stripping non-hex characters. Mirrors `_short_id`.
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
    /// says it actually took. A failure here leaves the interface set to the
    /// `wlan0` fallback and is logged loudly rather than silently accepted; the
    /// start path refuses separately if that fallback turns out to be the radio.
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
            interface: AP_IFACE.to_string(),
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

    /// Resolve the AP passphrase. Precedence: existing
    /// `/etc/ados/ap-passphrase` → configured `network.hotspot.password` → a
    /// freshly GENERATED per-unit value.
    ///
    /// The last step used to be a single built-in string shared by every unit
    /// ever shipped. One published default on every access point is not a
    /// secret; anyone within radio range of any ADOS ground station could join
    /// the network of any other.
    ///
    /// Generating instead is only safe because the value is now displayed —
    /// on the installer's completion summary, in the on-box status view, and
    /// through the console. Nothing showed it before, so a generated
    /// passphrase would have been undiscoverable and the unit unjoinable.
    /// If that display path is ever removed, this must go back with it.
    ///
    /// An explicitly configured passphrase still wins, so a fleet that wants
    /// one shared credential can still say so.
    pub fn ensure_passphrase(&mut self) -> String {
        if let Ok(existing) = std::fs::read_to_string(&self.passphrase_path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                self.passphrase = trimmed.to_string();
                info!(path = %self.passphrase_path.display(), "ap_passphrase_loaded");
                return self.passphrase.clone();
            }
        }
        let configured = self.configured_passphrase.trim();
        if !configured.is_empty() {
            self.passphrase = configured.to_string();
            info!("ap_passphrase_from_config");
            return self.passphrase.clone();
        }
        match ados_protocol::secret_gen::generate_ap_passphrase() {
            Ok(fresh) => {
                // Create EXCLUSIVELY, and adopt the winner on a collision.
                //
                // Several processes resolve this passphrase on a fresh boot —
                // the native AP manager, the Python one, and (until it was made
                // read-only) a status route. Each drew its own value and each
                // wrote the file, so the value an operator was shown could
                // differ from the one hostapd actually loaded, and the access
                // point was unjoinable. Ordering them is fragile; making the
                // create atomic is not. Whoever wins writes, everyone else
                // reads the winner, and all of them end up on one passphrase.
                if let Some(parent) = self.passphrase_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match create_new_with_mode(
                    &self.passphrase_path,
                    format!("{fresh}\n").as_bytes(),
                    0o600,
                ) {
                    Ok(()) => {
                        self.passphrase = fresh;
                        info!(
                            path = %self.passphrase_path.display(),
                            "ap_passphrase_generated"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Another process got there first. Its value is the one
                        // on disk and therefore the one every surface displays.
                        match std::fs::read_to_string(&self.passphrase_path) {
                            Ok(existing) if !existing.trim().is_empty() => {
                                self.passphrase = existing.trim().to_string();
                                info!("ap_passphrase_adopted_from_concurrent_writer");
                            }
                            _ => {
                                self.passphrase = fresh;
                                warn!("ap_passphrase_race_left_an_unreadable_file");
                            }
                        }
                    }
                    Err(e) => {
                        self.passphrase = fresh;
                        error!(
                            error = %e,
                            path = %self.passphrase_path.display(),
                            "ap_passphrase_generated_but_not_persisted"
                        );
                    }
                }
            }
            Err(e) => {
                // Actually fail closed. This branch used to substitute a single
                // passphrase compiled into every unit, which the comment above
                // it described as failing closed while doing the opposite.
                //
                // One published string shared by every ground station ever
                // shipped is worse than having no access point: the network
                // presents as protected, so nobody knows to distrust it.
                //
                // An empty passphrase stops `write_config` from emitting a
                // hostapd.conf, so the AP simply does not come up. A missing AP
                // is recoverable and visible; a fleet-wide known key is neither.
                self.passphrase.clear();
                error!(error = %e, "ap_passphrase_generate_failed_refusing_to_start_ap");
            }
        }
        self.passphrase.clone()
    }

    /// Render the hostapd.conf body. EXACT line order matches the Python
    /// `_render_hostapd_conf`; the body ends in a single trailing newline.
    pub fn render_hostapd_conf(&self) -> String {
        let lines = [
            format!("# ADOS Ground Station hostapd config for {}", self.ssid),
            format!("interface={}", self.interface),
            "driver=nl80211".to_string(),
            format!("ssid={}", self.ssid),
            "hw_mode=g".to_string(),
            format!("channel={}", self.channel),
            format!("country_code={}", self.country_code),
            "ieee80211n=1".to_string(),
            "ieee80211d=1".to_string(),
            "wmm_enabled=1".to_string(),
            "auth_algs=1".to_string(),
            "macaddr_acl=0".to_string(),
            "ignore_broadcast_ssid=0".to_string(),
            "wpa=2".to_string(),
            format!("wpa_passphrase={}", self.passphrase),
            "wpa_key_mgmt=WPA-PSK".to_string(),
            "wpa_pairwise=CCMP".to_string(),
            "rsn_pairwise=CCMP".to_string(),
            String::new(),
        ];
        lines.join("\n")
    }

    /// Render the dnsmasq conf body. EXACT line order matches the Python
    /// `_render_dnsmasq_conf`; single trailing newline.
    pub fn render_dnsmasq_conf(&self) -> String {
        let lines = [
            format!("# ADOS Ground Station DHCP for {}", self.interface),
            format!("interface={}", self.interface),
            "bind-interfaces".to_string(),
            "except-interface=lo".to_string(),
            format!("dhcp-range={DHCP_RANGE}"),
            format!("dhcp-option=3,{AP_ADDR}"),
            format!("dhcp-option=6,{AP_ADDR}"),
            "domain-needed".to_string(),
            "bogus-priv".to_string(),
            "no-resolv".to_string(),
            String::new(),
        ];
        lines.join("\n")
    }

    /// Render and write both conf files: hostapd 0600, dnsmasq 0644. Mirrors
    /// `write_config`. Ensures the passphrase before the first render.
    pub fn write_config(&mut self) -> std::io::Result<()> {
        if self.passphrase.is_empty() {
            self.ensure_passphrase();
        }
        // Still empty means the RNG failed and there is no passphrase to use.
        // Refuse here rather than emitting a conf: WPA requires 8-63 characters,
        // so an empty one either yields an open network or a start-time failure
        // from hostapd that reads as an unrelated fault.
        if self.passphrase.is_empty() {
            error!("ap_config_refused_no_passphrase");
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "refusing to write hostapd.conf without a passphrase",
            ));
        }
        // Never emit a conf that points hostapd at the aircraft's radio link.
        // The resolver should already have avoided it; this is the backstop for
        // the case where resolution failed and left the `wlan0` fallback in
        // place on a boot where `wlan0` IS the radio -- which is the exact
        // one-in-three ordering measured on the bench.
        if let Some(radio) = ados_protocol::netif::radio_interface() {
            if radio == self.interface {
                error!(
                    interface = %self.interface,
                    "ap_config_refused_interface_is_the_wfb_radio"
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "refusing to run the access point on {}: the WFB radio \
                         is using it",
                        self.interface
                    ),
                ));
            }
        }
        let hostapd_body = self.render_hostapd_conf();
        let dnsmasq_body = self.render_dnsmasq_conf();

        if let Some(parent) = self.hostapd_conf_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_with_mode(&self.hostapd_conf_path, hostapd_body.as_bytes(), 0o600).inspect_err(
            |exc| error!(path = %self.hostapd_conf_path.display(), error = %exc, "hostapd_conf_write_failed"),
        )?;
        write_with_mode(&self.dnsmasq_conf_path, dnsmasq_body.as_bytes(), 0o644).inspect_err(
            |exc| error!(path = %self.dnsmasq_conf_path.display(), error = %exc, "dnsmasq_conf_write_failed"),
        )?;

        info!(ssid = %self.ssid, channel = self.channel, "ap_config_written");
        Ok(())
    }

    async fn systemctl(&self, action: &str, unit: &str) -> bool {
        let out = self
            .runner
            .run(&["systemctl", action, unit], CMD_TIMEOUT)
            .await;
        if !out.ok() {
            warn!(
                action = action,
                unit = unit,
                rc = out.rc,
                "systemctl_nonzero"
            );
        }
        out.ok()
    }

    async fn assign_ip(&self) -> bool {
        // Idempotent: re-adding an existing address returns non-zero, swallowed.
        self.runner
            .run(
                &["ip", "addr", "add", AP_CIDR, "dev", &self.interface],
                SHORT_TIMEOUT,
            )
            .await;
        self.runner
            .run(&["ip", "link", "set", &self.interface, "up"], SHORT_TIMEOUT)
            .await;
        true
    }

    /// Release the AP gateway address from the AP interface. Idempotent: `ip
    /// addr del` returns non-zero when the address is already absent, which is
    /// swallowed. Only the `192.168.4.1/24` AP address is removed, so a wlan0
    /// that is (also) carrying a client uplink keeps its client IP. Used when
    /// standing the AP down so the AP gateway never lingers on the interface.
    pub async fn release_ip(&self) {
        self.runner
            .run(
                &["ip", "addr", "del", AP_CIDR, "dev", &self.interface],
                SHORT_TIMEOUT,
            )
            .await;
    }

    /// True when the hostapd unit is active. Thin public accessor over
    /// `is_unit_active` for the setup-AP guard's reconcile.
    pub async fn is_running(&self) -> bool {
        self.is_unit_active(HOSTAPD_UNIT).await
    }

    /// Bring the AP up: write configs, assign the gateway IP, start both units,
    /// then make the radio prove it.
    ///
    /// Returns whether the SSID is actually on the air — not whether
    /// `systemctl` exited zero. `ados-hostapd.service` is `Type=simple`, so
    /// systemctl returns the moment the fork succeeds and its exit code says
    /// nothing about whether hostapd got the phy into AP mode (or exited two
    /// seconds later on a bad conf). Every caller treats `false` as
    /// `ap_start_incomplete` and logs it, which is what the operator needs to
    /// see when the AP they are about to rely on did not come up.
    pub async fn start(&mut self) -> bool {
        if let Err(exc) = self.write_config() {
            error!(error = %exc, "ap_config_write_failed");
            return false;
        }
        self.assign_ip().await;
        // A fresh start invalidates any transmit baseline from the last run.
        *self.tx_sample.lock() = None;
        let hostapd_ok = self.systemctl("start", HOSTAPD_UNIT).await;
        let dnsmasq_ok = self.systemctl("start", DNSMASQ_UNIT).await;
        let radiating = match self.await_ap_mode().await {
            Some(proved) => proved,
            // The radio could not be asked at all (no `iw`, interface gone).
            // Fall back to the start verb's own outcome rather than claiming
            // either more or less than is known.
            None => {
                warn!(iface = %self.interface, "ap_liveness_unprobeable_at_start");
                hostapd_ok
            }
        };
        info!(
            hostapd = hostapd_ok,
            dnsmasq = dnsmasq_ok,
            radiating,
            ssid = %self.ssid,
            "ap_started"
        );
        radiating
    }

    /// Ask the radio whether the AP interface is serving a BSS.
    ///
    /// `Some(true)` proved, `Some(false)` disproved within the settle budget,
    /// `None` when the probe itself could not be made — which is never treated
    /// as a fault.
    async fn await_ap_mode(&self) -> Option<bool> {
        let deadline = Instant::now() + AP_SETTLE_BUDGET;
        loop {
            let radio = self.probe_radio().await;
            if !radio.probe_ok {
                return None;
            }
            if radio.iface_type.as_deref() == Some("AP") && radio.operating_channel.is_some() {
                return Some(true);
            }
            if Instant::now() >= deadline {
                warn!(
                    iface = %self.interface,
                    iface_type = ?radio.iface_type,
                    "ap_never_entered_ap_mode"
                );
                return Some(false);
            }
            tokio::time::sleep(AP_SETTLE_POLL).await;
        }
    }

    /// Tear the AP down. Best-effort on both units. Mirrors `stop`.
    pub async fn stop(&self) {
        self.systemctl("stop", DNSMASQ_UNIT).await;
        self.systemctl("stop", HOSTAPD_UNIT).await;
        info!("ap_stopped");
    }

    async fn is_unit_active(&self, unit: &str) -> bool {
        let out = self
            .runner
            .run(&["systemctl", "is-active", unit], SHORT_TIMEOUT)
            .await;
        out.stdout.trim() == "active"
    }

    /// Scrape `iw dev wlan0 station dump` for associated MAC addresses. Mirrors
    /// `_connected_clients`.
    async fn connected_clients(&self) -> Vec<String> {
        let out = self
            .runner
            .run(
                &["iw", "dev", &self.interface, "station", "dump"],
                SHORT_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return Vec::new();
        }
        let mut macs = Vec::new();
        for line in out.stdout.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Station ") {
                if let Some(mac) = rest.split_whitespace().next() {
                    macs.push(mac.to_lowercase());
                }
            }
        }
        macs
    }

    /// Ask the kernel what the AP interface is actually doing.
    async fn probe_radio(&self) -> RadioView {
        let out = self
            .runner
            .run(&["iw", "dev", &self.interface, "info"], SHORT_TIMEOUT)
            .await;
        if !out.ok() {
            return RadioView::default();
        }
        parse_iw_info(&out.stdout)
    }

    /// Cumulative frames the AP interface has handed to the driver, or `None`
    /// when the counter cannot be read.
    fn read_tx_packets(&self) -> Option<u64> {
        std::fs::read_to_string(
            self.sysfs_net_root
                .join(&self.interface)
                .join("statistics")
                .join("tx_packets"),
        )
        .ok()?
        .trim()
        .parse()
        .ok()
    }

    /// Fold one transmit-counter reading into the running baseline and report
    /// how long the counter has been flat, or `None` when there is no reading.
    ///
    /// An unreadable counter clears the baseline rather than carrying the old
    /// one forward: a gap in the samples is not elapsed flatness, and treating
    /// it as such would let a transient sysfs read failure age straight into a
    /// stall verdict.
    fn observe_tx(&self, reading: Option<u64>, now: Instant) -> Option<Duration> {
        let mut slot = self.tx_sample.lock();
        let Some(packets) = reading else {
            *slot = None;
            return None;
        };
        match slot.as_mut() {
            Some(prev) if prev.packets == packets => Some(now.duration_since(prev.flat_since)),
            Some(prev) => {
                prev.packets = packets;
                prev.flat_since = now;
                Some(Duration::ZERO)
            }
            None => {
                *slot = Some(TxSample {
                    packets,
                    flat_since: now,
                });
                Some(Duration::ZERO)
            }
        }
    }

    /// Judge an AP that is supposed to be up. Pure observation — no restart.
    ///
    /// Order matters: the interface mode is the load-bearing assertion and the
    /// transmit delta only ever corroborates it, because a beacon does not move
    /// the netdev counter (see [`TX_STALL_WINDOW`]).
    pub async fn check_liveness(&self) -> ApLiveness {
        let radio = self.probe_radio().await;
        if !radio.probe_ok {
            return ApLiveness::Unknown("radio_unprobeable");
        }
        if radio.iface_type.as_deref() != Some("AP") {
            return ApLiveness::Stalled("not_ap_mode");
        }
        let stations = self.connected_clients().await.len();
        let flat_for = self.observe_tx(self.read_tx_packets(), Instant::now());
        if radio.operating_channel.is_none() && stations == 0 {
            // In AP mode with no channel context and nobody associated: the
            // BSS never started, so nothing is being broadcast.
            return ApLiveness::Stalled("no_operating_channel");
        }
        if stations > 0 && flat_for.is_some_and(|d| d >= TX_STALL_WINDOW) {
            return ApLiveness::Stalled("tx_flat_with_stations");
        }
        ApLiveness::Radiating
    }

    /// One supervision pass over the AP, for the daemon's health tick.
    ///
    /// Restarts hostapd when the unit is active but the radio is not serving
    /// the AP. Only [`ApLiveness::Stalled`] acts; `Unknown` never does, so a
    /// box without `iw` keeps its AP instead of losing it to a probe gap. The
    /// unit being inactive is not this method's business — the setup-AP guard
    /// owns whether the AP should be up at all.
    pub async fn supervise(&self) -> ApLiveness {
        if !self.is_unit_active(HOSTAPD_UNIT).await {
            *self.tx_sample.lock() = None;
            return ApLiveness::Unknown("unit_inactive");
        }
        let verdict = self.check_liveness().await;
        if let ApLiveness::Stalled(reason) = verdict {
            warn!(
                reason,
                iface = %self.interface,
                ssid = %self.ssid,
                "ap_not_radiating_restarting_hostapd"
            );
            *self.tx_sample.lock() = None;
            self.systemctl("restart", HOSTAPD_UNIT).await;
        }
        verdict
    }

    /// Live AP status.
    ///
    /// `running` is derived from the RADIO, never from the unit: the unit being
    /// active only says a process exists. `hostapd_unit_active` carries the
    /// unit fact separately so a diagnosis can still tell "the daemon is dead"
    /// apart from "the daemon is alive and not serving the AP", and `probe_ok`
    /// keeps "could not ask the radio" distinct from "asked, and it is not an
    /// AP". Field-for-field parity with the Python manager's `status()`.
    pub async fn status(&self) -> Value {
        let unit_active = self.is_unit_active(HOSTAPD_UNIT).await;
        let radio = self.probe_radio().await;
        let in_ap_mode = radio.iface_type.as_deref() == Some("AP");
        let clients = if in_ap_mode {
            self.connected_clients().await
        } else {
            Vec::new()
        };
        let tx_packets = self.read_tx_packets();
        let flat_for = self.observe_tx(tx_packets, Instant::now());
        let running = radio.probe_ok
            && in_ap_mode
            && (radio.operating_channel.is_some() || !clients.is_empty());
        json!({
            "running": running,
            "ssid": self.ssid,
            "channel": self.channel,
            "interface": self.interface,
            "gateway": AP_ADDR,
            "connected_clients": clients,
            "hostapd_unit_active": unit_active,
            "radio": {
                "probe_ok": radio.probe_ok,
                "iface_type": radio.iface_type,
                "operating_channel": radio.operating_channel,
                "ssid": radio.ssid,
                "station_count": clients.len(),
                "tx_packets": tx_packets,
                "tx_flat_seconds": flat_for.map(|d| d.as_secs()),
            },
        })
    }

    /// Idempotent update. Restarts hostapd only when something changed. A
    /// passphrase update overwrites `/etc/ados/ap-passphrase` (0600 + trailing
    /// newline). Mirrors `apply_ap_config`.
    pub async fn apply_ap_config(
        &mut self,
        ssid: Option<&str>,
        passphrase: Option<&str>,
        channel: Option<u32>,
    ) -> bool {
        let mut changed = false;
        if let Some(s) = ssid {
            if s != self.ssid {
                self.ssid = s.to_string();
                changed = true;
            }
        }
        if let Some(c) = channel {
            if c != self.channel {
                self.channel = c;
                changed = true;
            }
        }
        if let Some(p) = passphrase {
            if p != self.passphrase {
                self.passphrase = p.to_string();
                if let Some(parent) = self.passphrase_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(exc) =
                    write_with_mode(&self.passphrase_path, format!("{p}\n").as_bytes(), 0o600)
                {
                    error!(error = %exc, "ap_passphrase_update_failed");
                    return false;
                }
                changed = true;
            }
        }
        if !changed {
            return true;
        }
        if let Err(exc) = self.write_config() {
            error!(error = %exc, "ap_config_write_failed");
            return false;
        }
        // A restart resets the interface's counters, so the old baseline would
        // read as a huge backwards jump and then as fresh flatness.
        *self.tx_sample.lock() = None;
        self.systemctl("restart", HOSTAPD_UNIT).await;
        info!(ssid = %self.ssid, channel = self.channel, "ap_config_applied");
        true
    }
}

/// Create `path` with `body` and `mode`, failing if it already exists.
///
/// `O_EXCL` is the point: it makes concurrent first-boot generation safe
/// without having to order the processes that do it. The caller adopts the
/// existing file on `AlreadyExists`, so every process converges on one value.
fn create_new_with_mode(path: &std::path::Path, body: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    f.write_all(body)?;
    f.sync_all()
}

/// Write `body` to `path` with an explicit unix mode (owner-controlled secret
/// files). Truncating, direct write (not atomic-rename — the Python writer also
/// writes in place + chmods).
fn write_with_mode(path: &std::path::Path, body: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    f.write_all(body)?;
    // create() only applies the mode on first creation; force it on rewrite too.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;
    use std::os::unix::fs::PermissionsExt;

    fn mgr(dir: &std::path::Path, device_id: &str, runner: Arc<ScriptedRunner>) -> HostapdManager {
        HostapdManager::with_paths(
            device_id,
            None,
            6,
            String::new(),
            runner,
            dir.join("hostapd-gs.conf"),
            dir.join("dnsmasq-gs.conf"),
            dir.join("ap-passphrase"),
        )
    }

    #[test]
    fn short_id_takes_four_hex_uppercased_and_pads() {
        // 'a' and 'd' from "ados" ARE hex, so the stripped string is
        // "ad58c27faf" → first four "ad58" → "AD58" (matches the Python regex).
        assert_eq!(short_id("ados-58c27faf"), "AD58");
        assert_eq!(build_ssid("ados-58c27faf"), "ADOS-GS-AD58");
        // A pure-hex id is taken verbatim.
        assert_eq!(short_id("58c27faf"), "58C2");
        // Short id pads with zeros.
        assert_eq!(short_id("ab"), "AB00");
        // Empty → all zeros.
        assert_eq!(short_id(""), "0000");
        // 'g' is not hex; only a/b/c/d/e/f + digits count.
        assert_eq!(short_id("ggggde12"), "DE12");
    }

    #[test]
    fn the_ap_country_follows_the_operators_pinned_region() {
        // It was a hardcoded IN while the radio's own reconciler defaulted to
        // US, so a stock box declared two different jurisdictions at once.
        assert_eq!(ados_protocol::ap_country::from_yaml(""), "US");
        assert_eq!(
            ados_protocol::ap_country::from_yaml(
                "network:\n  regulatory:\n    mode: region\n    region: IN\n"
            ),
            "IN",
            "an operator who pins a region still gets it"
        );
    }

    #[test]
    fn concurrent_first_boot_generation_converges_on_one_value() {
        // Several processes resolve this on a fresh boot — the native AP
        // manager, the Python one, and (before it was made read-only) a status
        // route. Each drew its own value and each wrote the file, so the value
        // an operator was shown could differ from the one hostapd loaded and
        // the access point was unjoinable. Ordering them is fragile; an
        // exclusive create is not.
        let dir = tempfile::tempdir().unwrap();
        let mut first = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        let mut second = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        let mut third = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));

        let a = first.ensure_passphrase();
        let b = second.ensure_passphrase();
        let c = third.ensure_passphrase();

        assert_eq!(a, b, "a second resolver must adopt the first's value");
        assert_eq!(b, c, "and so must a third");
        let on_disk = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
        assert_eq!(
            on_disk.trim(),
            a,
            "the displayed value must be the one on disk"
        );
    }

    #[test]
    fn a_configured_passphrase_wins_over_generating() {
        // The doc promised this and the path that runs made it unreachable: a
        // fleet setting one shared credential got a random value per box.
        let dir = tempfile::tempdir().unwrap();
        let mut m = HostapdManager::with_paths(
            "dead",
            None,
            6,
            "FleetShared2026".to_string(),
            Arc::new(ScriptedRunner::new()),
            dir.path().join("hostapd-gs.conf"),
            dir.path().join("dnsmasq-gs.conf"),
            dir.path().join("ap-passphrase"),
        );
        assert_eq!(m.ensure_passphrase(), "FleetShared2026");
    }

    #[test]
    fn a_generated_passphrase_is_stable_across_restarts() {
        // A generated value that is not written is a DIFFERENT passphrase on
        // every restart: the operator reads one off the installer card, the
        // service restarts, and the network they were told to join is gone.
        let dir = tempfile::tempdir().unwrap();
        let mut first = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        let generated = first.ensure_passphrase();

        // Same box, fresh manager — as after a service restart.
        let mut again = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        assert_eq!(
            again.ensure_passphrase(),
            generated,
            "the generated passphrase must survive a restart"
        );

        let on_disk = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
        assert_eq!(on_disk, format!("{generated}\n"));
    }

    #[test]
    fn a_generated_passphrase_is_persisted_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut m = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        m.ensure_passphrase();
        let mode = std::fs::metadata(dir.path().join("ap-passphrase"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the passphrase file must not be world-readable"
        );
    }

    #[test]
    fn ensure_passphrase_precedence_file_then_config_then_generated() {
        let dir = tempfile::tempdir().unwrap();
        // No file, no config → a fresh per-unit value, NOT the shared builtin.
        // One published default across every shipped unit is not a secret:
        // anyone in radio range of any ground station could join any other.
        let mut m = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        let generated = m.ensure_passphrase();
        assert_ne!(
            generated, "altnautica",
            "a fresh unit must not come up on the shared built-in passphrase"
        );
        assert!(
            ados_protocol::secret_gen::is_valid_wpa2_passphrase(&generated),
            "hostapd refuses the whole config on an illegal passphrase"
        );

        // And it is per-unit: a second box does not get the same one.
        let dir2 = tempfile::tempdir().unwrap();
        let mut other = mgr(dir2.path(), "beef", Arc::new(ScriptedRunner::new()));
        assert_ne!(
            other.ensure_passphrase(),
            generated,
            "two units must not share a generated passphrase"
        );

        // Configured password wins over builtin (no file present).
        let mut m2 = HostapdManager::with_paths(
            "dead",
            None,
            6,
            "configured-pw".to_string(),
            Arc::new(ScriptedRunner::new()),
            dir.path().join("h2.conf"),
            dir.path().join("d2.conf"),
            dir.path().join("ap-passphrase-2"),
        );
        assert_eq!(m2.ensure_passphrase(), "configured-pw");

        // Existing file wins over everything.
        std::fs::write(dir.path().join("ap-passphrase"), "from-file\n").unwrap();
        let mut m3 = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        assert_eq!(m3.ensure_passphrase(), "from-file");
    }

    #[test]
    fn a_written_conf_always_carries_a_wpa2_legal_passphrase() {
        // The invariant behind the fail-closed guard in `write_config`: no
        // hostapd.conf is ever emitted with an empty or illegal passphrase,
        // which would either open the network or make hostapd fail at start
        // for reasons that read as an unrelated fault.
        //
        // Honest limit: this exercises the success path. The RNG-failure branch
        // that leaves the passphrase empty is not reachable from a test without
        // injecting a `getrandom` failure, so the guard itself is defence in
        // depth rather than something proven here.
        let dir = tempfile::tempdir().unwrap();
        let mut m = mgr(dir.path(), "c0ffee", Arc::new(ScriptedRunner::new()));
        m.country_code = "US".to_string();
        m.write_config().unwrap();

        let body = std::fs::read_to_string(dir.path().join("hostapd-gs.conf")).unwrap();
        let line = body
            .lines()
            .find(|l| l.starts_with("wpa_passphrase="))
            .expect("a written conf must set wpa_passphrase");
        let value = line.trim_start_matches("wpa_passphrase=");
        assert!(
            !value.is_empty(),
            "an empty passphrase must never be written"
        );
        assert!(
            ados_protocol::secret_gen::is_valid_wpa2_passphrase(value),
            "written passphrase must satisfy WPA2's 8-63 character rule"
        );
    }

    #[test]
    fn hostapd_conf_is_byte_exact_with_0600_mode() {
        let dir = tempfile::tempdir().unwrap();
        // Pin the passphrase through config so the golden body stays exact:
        // an unconfigured unit now generates a fresh one per box.
        let mut m = HostapdManager::with_paths(
            "58c27faf",
            None,
            6,
            "altnautica".to_string(),
            Arc::new(ScriptedRunner::new()),
            dir.path().join("hostapd-gs.conf"),
            dir.path().join("dnsmasq-gs.conf"),
            dir.path().join("ap-passphrase"),
        );
        m.ensure_passphrase(); // → the configured "altnautica"
                               // Pin the country so the golden body does not depend on the host's
                               // /etc/ados/config.yaml. An unpinned unit resolves to the same default.
        m.country_code = "US".to_string();
        m.write_config().unwrap();

        let expected = "# ADOS Ground Station hostapd config for ADOS-GS-58C2\n\
interface=wlan0\n\
driver=nl80211\n\
ssid=ADOS-GS-58C2\n\
hw_mode=g\n\
channel=6\n\
country_code=US\n\
ieee80211n=1\n\
ieee80211d=1\n\
wmm_enabled=1\n\
auth_algs=1\n\
macaddr_acl=0\n\
ignore_broadcast_ssid=0\n\
wpa=2\n\
wpa_passphrase=altnautica\n\
wpa_key_mgmt=WPA-PSK\n\
wpa_pairwise=CCMP\n\
rsn_pairwise=CCMP\n";
        let body = std::fs::read_to_string(dir.path().join("hostapd-gs.conf")).unwrap();
        assert_eq!(body, expected);
        let mode = std::fs::metadata(dir.path().join("hostapd-gs.conf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn dnsmasq_conf_is_byte_exact_with_0644_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = mgr(dir.path(), "58c27faf", Arc::new(ScriptedRunner::new()));
        m.ensure_passphrase();
        m.write_config().unwrap();

        let expected = "# ADOS Ground Station DHCP for wlan0\n\
interface=wlan0\n\
bind-interfaces\n\
except-interface=lo\n\
dhcp-range=192.168.4.10,192.168.4.100,12h\n\
dhcp-option=3,192.168.4.1\n\
dhcp-option=6,192.168.4.1\n\
domain-needed\n\
bogus-priv\n\
no-resolv\n";
        let body = std::fs::read_to_string(dir.path().join("dnsmasq-gs.conf")).unwrap();
        assert_eq!(body, expected);
        let mode = std::fs::metadata(dir.path().join("dnsmasq-gs.conf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[tokio::test]
    async fn apply_ap_config_writes_passphrase_0600_with_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        // write_config writes the two confs; restart is one systemctl call.
        let mut m = mgr(dir.path(), "58c27faf", runner.clone());
        m.ensure_passphrase();
        let ok = m.apply_ap_config(None, Some("new-secret"), None).await;
        assert!(ok);
        let pw = std::fs::read_to_string(dir.path().join("ap-passphrase")).unwrap();
        assert_eq!(pw, "new-secret\n");
        let mode = std::fs::metadata(dir.path().join("ap-passphrase"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        // It issued a hostapd restart.
        assert!(runner
            .recorded()
            .iter()
            .any(|c| c.contains(&"restart".to_string())));
    }

    /// `iw dev <iface> info` for a ground station whose AP is up.
    fn iw_info_ap(channel: Option<u32>) -> String {
        let mut s = String::from(
            "Interface wlan0\n\tifindex 3\n\twdev 0x1\n\taddr b8:27:eb:11:22:33\n\
             \tssid ADOS-GS-58C2\n\ttype AP\n\twiphy 0\n",
        );
        if let Some(c) = channel {
            s.push_str(&format!(
                "\tchannel {c} (2437 MHz), width: 20 MHz, center1: 2437 MHz\n"
            ));
        }
        s.push_str("\ttxpower 31.00 dBm\n");
        s
    }

    /// Seed a fake `/sys/class/net/<iface>/statistics/tx_packets`.
    fn seed_tx(root: &std::path::Path, iface: &str, packets: u64) {
        let dir = root.join(iface).join("statistics");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tx_packets"), format!("{packets}\n")).unwrap();
    }

    #[test]
    fn iw_info_parses_mode_channel_and_ssid_and_flags_an_unusable_answer() {
        let up = parse_iw_info(&iw_info_ap(Some(6)));
        assert!(up.probe_ok);
        assert_eq!(up.iface_type.as_deref(), Some("AP"));
        assert_eq!(up.operating_channel, Some(6));
        assert_eq!(up.ssid.as_deref(), Some("ADOS-GS-58C2"));

        // In AP mode but with no channel context: hostapd is alive, the BSS is
        // not. That distinction is the whole point of reading the channel line.
        let idle = parse_iw_info(&iw_info_ap(None));
        assert!(idle.probe_ok);
        assert_eq!(idle.operating_channel, None);

        // No `type` line at all: the probe did not answer. It must NOT read as
        // "answered, and it is not an AP" — nothing may be concluded from it.
        let unusable = parse_iw_info("command failed: No such device (-19)\n");
        assert!(!unusable.probe_ok);
        assert_eq!(unusable.iface_type, None);
    }

    #[tokio::test]
    async fn status_reports_running_from_the_radio_not_from_the_unit() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // systemctl is-active → active
        runner.push(CmdOut {
            rc: 0,
            stdout: iw_info_ap(Some(6)),
            stderr: String::new(),
        }); // iw dev wlan0 info
        runner.push(CmdOut {
            rc: 0,
            stdout: "Station AA:BB:CC:DD:EE:FF (on wlan0)\n\tinactive time:\t10 ms\nStation 11:22:33:44:55:66 (on wlan0)\n".to_string(),
            stderr: String::new(),
        }); // iw dev wlan0 station dump
        let mut m = mgr(dir.path(), "58c27faf", runner);
        m.set_sysfs_net_root(dir.path().to_path_buf());
        seed_tx(dir.path(), "wlan0", 4200);

        let st = m.status().await;
        assert_eq!(st["running"], true);
        assert_eq!(st["hostapd_unit_active"], true);
        assert_eq!(st["radio"]["probe_ok"], true);
        assert_eq!(st["radio"]["iface_type"], "AP");
        assert_eq!(st["radio"]["operating_channel"], 6);
        assert_eq!(st["radio"]["tx_packets"], 4200);
        let clients = st["connected_clients"].as_array().unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0], "aa:bb:cc:dd:ee:ff");
        assert_eq!(clients[1], "11:22:33:44:55:66");
    }

    #[tokio::test]
    async fn an_active_unit_whose_radio_is_not_an_ap_reports_not_running() {
        // The defect this whole change exists for: the unit is `active` and no
        // SSID is on the air. A status surface that answered `running: true`
        // here is the one that hid a dead AP on every ground station.
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "Interface wlan0\n\tifindex 3\n\ttype managed\n".to_string(),
            stderr: String::new(),
        });
        let mut m = mgr(dir.path(), "58c27faf", runner);
        m.set_sysfs_net_root(dir.path().to_path_buf());

        let st = m.status().await;
        assert_eq!(st["running"], false);
        // …while still saying plainly that the daemon itself is up, so the
        // operator can tell "hostapd died" from "hostapd is not serving".
        assert_eq!(st["hostapd_unit_active"], true);
        assert_eq!(st["radio"]["iface_type"], "managed");
        // Nothing was scraped for stations: the interface is not an AP.
        assert_eq!(st["connected_clients"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn supervise_restarts_hostapd_when_the_radio_is_not_serving_the_ap() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is-active
        runner.push(CmdOut {
            rc: 0,
            stdout: "Interface wlan0\n\ttype managed\n".to_string(),
            stderr: String::new(),
        }); // iw info → not an AP
        let mut m = mgr(dir.path(), "58c27faf", runner.clone());
        m.set_sysfs_net_root(dir.path().to_path_buf());

        assert_eq!(m.supervise().await, ApLiveness::Stalled("not_ap_mode"));
        assert!(
            runner.recorded().iter().any(
                |c| c.contains(&"restart".to_string()) && c.contains(&HOSTAPD_UNIT.to_string())
            ),
            "a wedged AP was not restarted: {:?}",
            runner.recorded()
        );
    }

    #[tokio::test]
    async fn supervise_never_restarts_on_a_probe_it_could_not_make() {
        // No `iw`, or the interface is gone. Acting on that would turn a
        // missing tool into a restart loop of the operator's only reachability.
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is-active
        runner.push(CmdOut::failed(1, "command failed: No such device (-19)")); // iw info
        let mut m = mgr(dir.path(), "58c27faf", runner.clone());
        m.set_sysfs_net_root(dir.path().to_path_buf());

        assert_eq!(
            m.supervise().await,
            ApLiveness::Unknown("radio_unprobeable")
        );
        assert!(
            !runner
                .recorded()
                .iter()
                .any(|c| c.contains(&"restart".to_string())),
            "an unprobeable radio must not trigger a restart: {:?}",
            runner.recorded()
        );
    }

    #[test]
    fn a_flat_transmit_counter_accumulates_and_a_moving_one_resets_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        let m = mgr(dir.path(), "58c27faf", runner);
        let t0 = Instant::now();

        // First reading only establishes the baseline.
        assert_eq!(m.observe_tx(Some(100), t0), Some(Duration::ZERO));
        // Same value later: that elapsed time IS the flat window.
        assert_eq!(
            m.observe_tx(Some(100), t0 + Duration::from_secs(31)),
            Some(Duration::from_secs(31))
        );
        // The counter moves: the window restarts from zero.
        assert_eq!(
            m.observe_tx(Some(101), t0 + Duration::from_secs(32)),
            Some(Duration::ZERO)
        );
        // An unreadable counter is a GAP, not elapsed flatness: it clears the
        // baseline so a transient read failure cannot age into a stall verdict.
        assert_eq!(m.observe_tx(None, t0 + Duration::from_secs(33)), None);
        assert_eq!(
            m.observe_tx(Some(101), t0 + Duration::from_secs(99)),
            Some(Duration::ZERO)
        );
    }

    #[tokio::test]
    async fn a_flat_counter_with_no_station_is_not_a_stall() {
        // mac80211 does not account beacons into the netdev counter, so an idle
        // AP with nobody associated legitimately sits flat forever. Condemning
        // it would restart a healthy AP every window.
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: iw_info_ap(Some(6)),
            stderr: String::new(),
        }); // iw info → AP on channel 6
        runner.push(CmdOut {
            rc: 0,
            stdout: String::new(),
            stderr: String::new(),
        }); // station dump → nobody associated
        let mut m = mgr(dir.path(), "58c27faf", runner);
        m.set_sysfs_net_root(dir.path().to_path_buf());
        seed_tx(dir.path(), "wlan0", 7);
        // Age the baseline well past the stall window.
        *m.tx_sample.lock() = Some(TxSample {
            packets: 7,
            flat_since: Instant::now() - TX_STALL_WINDOW * 4,
        });

        assert_eq!(m.check_liveness().await, ApLiveness::Radiating);
    }

    #[tokio::test]
    async fn a_flat_counter_with_an_associated_station_is_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: iw_info_ap(Some(6)),
            stderr: String::new(),
        }); // iw info → AP on channel 6
        runner.push(CmdOut {
            rc: 0,
            stdout: "Station aa:bb:cc:dd:ee:ff (on wlan0)\n".to_string(),
            stderr: String::new(),
        }); // station dump → one client
        let mut m = mgr(dir.path(), "58c27faf", runner);
        m.set_sysfs_net_root(dir.path().to_path_buf());
        seed_tx(dir.path(), "wlan0", 7);
        *m.tx_sample.lock() = Some(TxSample {
            packets: 7,
            flat_since: Instant::now() - TX_STALL_WINDOW * 4,
        });

        assert_eq!(
            m.check_liveness().await,
            ApLiveness::Stalled("tx_flat_with_stations")
        );
    }
}
