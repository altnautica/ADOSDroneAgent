//! Joining and leaving a network, with AP hand-off and restore.

use super::*;
use crate::nmcli;
use serde_json::{json, Value};
use tracing::info;

impl WifiClientManager {
    /// Join a WiFi network, coordinating the station radio with hostapd.
    ///
    /// Every refusal names the interface it is talking about. The station is
    /// resolved by driver, not by name, so an operator reading `station_locked`
    /// cannot otherwise tell which radio is held.
    pub async fn join(&mut self, ssid: &str, passphrase: Option<&str>, force: bool) -> Value {
        if ssid.is_empty() {
            return json!({"joined": false, "error": "ssid_required", "ip": null, "gateway": null});
        }
        if self.interface.is_empty() {
            return json!({
                "joined": false,
                "error": "no_station_interface",
                "hint": "No onboard WiFi interface was found",
                "interface": null,
                "ip": null,
                "gateway": null,
            });
        }
        // Never run nmcli on the aircraft's flight radio, whatever the name.
        if ados_protocol::netif::radio_interface().as_deref() == Some(self.interface.as_str()) {
            return json!({
                "joined": false,
                "error": "station_is_wfb_radio",
                "interface": self.interface,
                "ip": null,
                "gateway": null,
            });
        }

        let ap_active = self.is_hostapd_active().await;
        if ap_active && !force {
            return json!({
                "joined": false,
                "error": "station_busy_ap_active",
                "hint": "Stop AP first or force",
                "interface": self.interface,
                "ip": null,
                "gateway": null,
            });
        }

        let fresh_lock = !self.holds_lock();
        if !self.acquire_lock() {
            return json!({
                "joined": false,
                "error": "station_locked",
                "interface": self.interface,
                "ip": null,
                "gateway": null,
            });
        }

        // Record whether the AP was up so leave() (or a failed join) restores
        // it. A rejoin while already holding the station keeps the record of
        // the first join: hostapd is down now because that join stopped it.
        if fresh_lock || ap_active {
            self.write_ap_flag(ap_active);
        }
        if ap_active {
            info!(ssid = ssid, "stopping_hostapd_for_client");
            self.runner
                .run(&["systemctl", "stop", HOSTAPD_UNIT], SYSTEMCTL_TIMEOUT)
                .await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // Keep the passphrase OUT of argv: a cleartext `password <pw>` argument
        // would be visible in /proc/<pid>/cmdline to any local user for the life
        // of the nmcli call. Instead, run nmcli in interactive-secret mode
        // (`--ask`) which prompts for the secret and reads it from stdin, and
        // feed the passphrase on stdin. With no secret needed (open network)
        // the argv is unchanged and no stdin is sent.
        let out = match passphrase.filter(|p| !p.is_empty()) {
            Some(pw) => {
                let cmd: Vec<&str> = vec![
                    "nmcli",
                    "--ask",
                    "device",
                    "wifi",
                    "connect",
                    ssid,
                    "ifname",
                    &self.interface,
                ];
                // nmcli --ask reads the WiFi secret as a line from stdin; the
                // trailing newline terminates the prompt response.
                let mut secret = pw.as_bytes().to_vec();
                secret.push(b'\n');
                let res = self
                    .runner
                    .run_with_stdin(&cmd, &secret, CONNECT_TIMEOUT)
                    .await;
                // Zeroize our copy of the secret promptly.
                secret.fill(0);
                res
            }
            None => {
                let cmd: Vec<&str> = vec![
                    "nmcli",
                    "device",
                    "wifi",
                    "connect",
                    ssid,
                    "ifname",
                    &self.interface,
                ];
                self.runner.run(&cmd, CONNECT_TIMEOUT).await
            }
        };
        if !out.ok() {
            warn!(ssid = ssid, "wifi_join_failed");
            // Restore the AP if a join took it, then release the lock.
            self.restore_ap_if_taken().await;
            self.release_lock();
            return json!({
                "joined": false,
                "error": err_or(&out.stderr, "nmcli_failed"),
                "ip": null,
                "gateway": null,
            });
        }

        // Force power-save off so the radio never parks and drops the uplink.
        // Resolve the real active connection name on this iface first, because
        // NetworkManager may auto-name the connection differently from the SSID
        // (so `connection modify <ssid>` would warn-and-no-op). Fall back to the
        // SSID when no active connection is reported.
        let conn = self
            .active_connection_name()
            .await
            .unwrap_or_else(|| ssid.to_string());
        self.disable_powersave(&conn).await;

        tokio::time::sleep(Duration::from_secs(2)).await;
        let st = self.status().await;
        let mut data = self.load_client_config();
        data.last_ssid = Some(ssid.to_string());
        self.save_client_config(&data);

        // Join succeeded. The lock stays held (we own the radio in STA mode);
        // it is released by leave().
        json!({
            "joined": true,
            "error": null,
            "ip": st.get("ip").cloned().unwrap_or(Value::Null),
            "gateway": st.get("gateway").cloned().unwrap_or(Value::Null),
        })
    }

    /// Resolve the NetworkManager connection name currently active on this
    /// iface (`nmcli -t -f NAME,DEVICE connection show --active`). Returns the
    /// first row whose DEVICE matches the managed interface. `None` when no
    /// active connection is bound to the iface (or the query failed).
    async fn active_connection_name(&self) -> Option<String> {
        let out = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "NAME,DEVICE",
                    "connection",
                    "show",
                    "--active",
                ],
                STATUS_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return None;
        }
        for row in nmcli::parse_terse(&out.stdout, 2) {
            if row[1].trim() == self.interface {
                let name = row[0].trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    async fn disable_powersave(&self, connection: &str) {
        let nm = self
            .runner
            .run(
                &[
                    "nmcli",
                    "connection",
                    "modify",
                    connection,
                    "802-11-wireless.powersave",
                    "2",
                ],
                RUN_TIMEOUT,
            )
            .await;
        if !nm.ok() {
            warn!(connection = connection, "wifi_powersave_nmcli_failed");
        }
        let iw = self
            .runner
            .run(
                &["iw", "dev", &self.interface, "set", "power_save", "off"],
                RUN_TIMEOUT,
            )
            .await;
        if !iw.ok() {
            warn!(interface = %self.interface, "wifi_powersave_iw_failed");
        }
    }

    /// Start hostapd again when a join recorded that it was up, and clear the
    /// record.
    async fn restore_ap_if_taken(&self) {
        if self.read_ap_flag() {
            info!("restoring_hostapd_after_client");
            self.runner
                .run(&["systemctl", "start", HOSTAPD_UNIT], SYSTEMCTL_TIMEOUT)
                .await;
        }
        self.clear_ap_flag();
    }

    /// Disconnect the current WiFi client connection, restore the AP if it was
    /// ours, and release the station lock. The AP is restored even when no
    /// client link is active any more (the joined network went away), so the
    /// node is never left with neither uplink nor its AP.
    pub async fn leave(&mut self) -> Value {
        let st = self.status().await;
        let prev_ssid = st.get("ssid").and_then(|v| v.as_str()).map(str::to_string);

        if let Some(prev) = prev_ssid.as_deref() {
            let down = self
                .runner
                .run(&["nmcli", "connection", "down", prev], SYSTEMCTL_TIMEOUT)
                .await;
            if !down.ok() {
                // Fallback: disconnect the device.
                self.runner
                    .run(
                        &["nmcli", "device", "disconnect", &self.interface],
                        SYSTEMCTL_TIMEOUT,
                    )
                    .await;
            }
        }

        self.restore_ap_if_taken().await;
        self.release_lock();

        json!({"left": prev_ssid.is_some(), "previous_ssid": prev_ssid})
    }
}
