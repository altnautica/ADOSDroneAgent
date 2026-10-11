//! Modem status, usage, config, and iface queries.

use super::*;

impl ModemManager {
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
    pub(super) fn current_iface(&self) -> String {
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
    pub(super) fn iface_up(&self) -> bool {
        let Some(iface) = self.cellular_iface() else {
            return false;
        };
        std::fs::read_to_string(self.net_dir.join(&iface).join("operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false)
    }
}
