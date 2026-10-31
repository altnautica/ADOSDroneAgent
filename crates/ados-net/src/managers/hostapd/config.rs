//! Passphrase resolution, config rendering and AP config changes.

use super::*;

impl HostapdManager {
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
                // Create EXCLUSIVELY, and adopt the winner on a collision, so
                // every process that resolves the passphrase on a fresh boot
                // ends up on the value hostapd actually loads: whoever wins
                // writes, everyone else reads the winner.
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

    /// Render the hostapd.conf body in a fixed line order; the body ends in a
    /// single trailing newline.
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

    /// Render the dnsmasq conf body in a fixed line order; single trailing
    /// newline.
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

    /// Render and write both conf files: hostapd 0600, dnsmasq 0644. Ensures
    /// the passphrase before the first render.
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
        if self.interface.is_empty() {
            error!("ap_config_refused_interface_unresolved");
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "refusing to write hostapd.conf: no onboard WiFi interface resolved",
            ));
        }
        // The settings are interpolated raw, so they are checked at render
        // time too: a passphrase loaded from disk or config never went through
        // the apply path.
        if let Err(reason) =
            validate_ap_settings(Some(&self.ssid), Some(&self.passphrase), Some(self.channel))
        {
            error!(reason = %reason, "ap_config_refused_invalid_settings");
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, reason));
        }
        // Never emit a conf that points hostapd at the aircraft's radio link.
        // The resolver should already have avoided it; this is the backstop for
        // a boot where the radio took the interface the AP resolved to.
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

    /// Idempotent update. Restarts hostapd only when something changed. A
    /// passphrase update overwrites `/etc/ados/ap-passphrase` (0600 + trailing
    /// newline). `false` when the settings are refused, a write fails, or the
    /// hostapd restart does not succeed: the new settings are then on disk but
    /// not on the air, and the caller must not report them as applied.
    pub async fn apply_ap_config(
        &mut self,
        ssid: Option<&str>,
        passphrase: Option<&str>,
        channel: Option<u32>,
    ) -> bool {
        if let Err(reason) = validate_ap_settings(ssid, passphrase, channel) {
            error!(reason = %reason, "ap_config_refused_invalid_settings");
            return false;
        }
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
        if !self.systemctl("restart", HOSTAPD_UNIT).await {
            error!(ssid = %self.ssid, channel = self.channel, "ap_config_restart_failed");
            return false;
        }
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
/// files). Truncating, direct write followed by a chmod, not an atomic rename.
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
