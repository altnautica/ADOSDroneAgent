//! Unit control and interface addressing for the access point.

use super::*;

impl HostapdManager {
    pub(super) async fn systemctl(&self, action: &str, unit: &str) -> bool {
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

    /// Tear the AP down. Best-effort on both units.
    pub async fn stop(&self) {
        self.systemctl("stop", DNSMASQ_UNIT).await;
        self.systemctl("stop", HOSTAPD_UNIT).await;
        info!("ap_stopped");
    }

    pub(super) async fn is_unit_active(&self, unit: &str) -> bool {
        let out = self
            .runner
            .run(&["systemctl", "is-active", unit], SHORT_TIMEOUT)
            .await;
        out.stdout.trim() == "active"
    }
}
