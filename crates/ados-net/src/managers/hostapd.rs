//! WiFi AP lifecycle for the ground-station profile.
//!
//! Runs `hostapd` on the onboard wlan0 so phones, tablets, and laptops join a
//! stable SSID (`ADOS-GS-<short_id>`) and reach the setup webapp, WHEP video,
//! and agent REST API. A matching `dnsmasq` serves DHCP on 192.168.4.0/24. The
//! RTL8812 USB adapter is reserved for monitor-mode WFB-ng RX elsewhere and is
//! never touched here. Ports `hostapd_manager.py`. Solo-benchable: config
//! rendering + passphrase resolution need no radio; start/stop are systemctl
//! calls through the injectable command runner.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
        Self::with_paths(
            device_id,
            ssid,
            channel,
            configured_passphrase,
            runner,
            PathBuf::from(crate::paths::HOSTAPD_CONF_PATH),
            PathBuf::from(crate::paths::DNSMASQ_CONF_PATH),
            PathBuf::from(crate::paths::AP_PASSPHRASE_PATH),
        )
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
        }
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

    /// Bring the AP up: write configs, assign the gateway IP, start both units.
    /// Mirrors `start`. Returns whether hostapd started.
    pub async fn start(&mut self) -> bool {
        if let Err(exc) = self.write_config() {
            error!(error = %exc, "ap_config_write_failed");
            return false;
        }
        self.assign_ip().await;
        let hostapd_ok = self.systemctl("start", HOSTAPD_UNIT).await;
        let dnsmasq_ok = self.systemctl("start", DNSMASQ_UNIT).await;
        info!(hostapd = hostapd_ok, dnsmasq = dnsmasq_ok, ssid = %self.ssid, "ap_started");
        hostapd_ok
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

    /// Live AP status. Mirrors `status`.
    pub async fn status(&self) -> Value {
        let running = self.is_unit_active(HOSTAPD_UNIT).await;
        let clients = if running {
            self.connected_clients().await
        } else {
            Vec::new()
        };
        json!({
            "running": running,
            "ssid": self.ssid,
            "channel": self.channel,
            "interface": self.interface,
            "gateway": AP_ADDR,
            "connected_clients": clients,
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
        assert!(!value.is_empty(), "an empty passphrase must never be written");
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

    #[tokio::test]
    async fn status_scrapes_station_dump_macs() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is-active → active
        runner.push(CmdOut {
            rc: 0,
            stdout: "Station AA:BB:CC:DD:EE:FF (on wlan0)\n\tinactive time:\t10 ms\nStation 11:22:33:44:55:66 (on wlan0)\n".to_string(),
            stderr: String::new(),
        }); // iw station dump
        let m = mgr(dir.path(), "58c27faf", runner);
        let st = m.status().await;
        assert_eq!(st["running"], true);
        let clients = st["connected_clients"].as_array().unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0], "aa:bb:cc:dd:ee:ff");
        assert_eq!(clients[1], "11:22:33:44:55:66");
    }
}
