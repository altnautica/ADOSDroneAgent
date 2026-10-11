//! Modem data-session bring-up / tear-down.

use super::*;

impl ModemManager {
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

    pub(super) fn register_dbus_failure(&self, st: &mut ModemState, reason: &str) {
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

    pub(super) fn register_dbus_success(&self, st: &mut ModemState) {
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
    pub(super) async fn apply_static_ip4(&self, iface: &str, cfg: &Ip4Static) {
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

    pub(super) fn set_bearer_iface(&self, iface: Option<String>) {
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
}
