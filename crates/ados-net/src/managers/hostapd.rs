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
const BUILTIN_PASSPHRASE: &str = "altnautica";

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

    /// Resolve the AP passphrase. Precedence: existing `/etc/ados/ap-passphrase`
    /// → configured `network.hotspot.password` → builtin `"altnautica"`. The
    /// agent NEVER auto-generates. Mirrors `ensure_passphrase`.
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
        if configured.is_empty() {
            warn!("ap_passphrase_using_builtin_default");
            self.passphrase = BUILTIN_PASSPHRASE.to_string();
        } else {
            self.passphrase = configured.to_string();
        }
        info!("ap_passphrase_from_config");
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
            "country_code=IN".to_string(),
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
    fn ensure_passphrase_precedence_file_then_config_then_builtin() {
        let dir = tempfile::tempdir().unwrap();
        // No file, no config → builtin.
        let mut m = mgr(dir.path(), "dead", Arc::new(ScriptedRunner::new()));
        assert_eq!(m.ensure_passphrase(), "altnautica");

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
    fn hostapd_conf_is_byte_exact_with_0600_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = mgr(dir.path(), "58c27faf", Arc::new(ScriptedRunner::new()));
        m.ensure_passphrase(); // → "altnautica"
        m.write_config().unwrap();

        let expected = "# ADOS Ground Station hostapd config for ADOS-GS-58C2\n\
interface=wlan0\n\
driver=nl80211\n\
ssid=ADOS-GS-58C2\n\
hw_mode=g\n\
channel=6\n\
country_code=IN\n\
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
