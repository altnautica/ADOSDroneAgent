//! Liveness probing, TX-stall detection and the status view.

use super::*;

impl HostapdManager {
    /// Scrape `iw dev wlan0 station dump` for associated MAC addresses.
    pub(super) async fn connected_clients(&self) -> Vec<String> {
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
    pub(super) async fn probe_radio(&self) -> RadioView {
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
    pub(super) fn observe_tx(&self, reading: Option<u64>, now: Instant) -> Option<Duration> {
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
    /// the netdev counter (see `TX_STALL_WINDOW`).
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
            return ApLiveness::Down;
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
    /// AP".
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
            "interface": (!self.interface.is_empty()).then_some(&self.interface),
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
}
