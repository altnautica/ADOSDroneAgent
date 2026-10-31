//! Scanning, saved-profile management and the status view.

use super::*;
use crate::nmcli;
use serde_json::{json, Value};

impl WifiClientManager {
    /// Scan for nearby networks on the station interface. When hostapd holds
    /// the radio the scan runs in `ap-force` mode so the AP stays up.
    pub async fn scan(&self) -> Result<Vec<Value>, String> {
        if self.interface.is_empty() {
            return Err("no station radio".to_string());
        }
        let ap_active = self.is_hostapd_active().await;
        crate::iw_scan::scan(self.runner.as_ref(), &self.interface, ap_active).await
    }

    /// Delete a saved NetworkManager connection profile by name. Removes the
    /// stored credentials + the autoconnect profile so the agent never silently
    /// rejoins on reboot; the active link drops as a side effect when the deleted
    /// profile is the currently-active one. Mirrors `forget`. Stateless (`&self`,
    /// no lock): deleting a saved profile does not change AP/STA radio ownership.
    pub async fn forget(&self, connection_name: &str) -> Value {
        if connection_name.is_empty() {
            return json!({"forgot": false, "name": "", "error": "name_required"});
        }
        let out = self
            .runner
            .run(
                &["nmcli", "connection", "delete", connection_name],
                SYSTEMCTL_TIMEOUT,
            )
            .await;
        if out.ok() {
            json!({"forgot": true, "name": connection_name, "error": null})
        } else {
            json!({"forgot": false, "name": connection_name, "error": "nmcli_failed"})
        }
    }

    /// Toggle the NetworkManager autoconnect flag for a saved profile. An empty
    /// name is `{"autoconnect": false, "name": "", "error": "name_required"}`;
    /// otherwise it runs `nmcli connection modify <name> connection.autoconnect
    /// yes|no` and returns
    /// `{"autoconnect": <enabled>, "name": <name>, "error": null}` on success, or
    /// the trimmed stderr/stdout (falling back to `nmcli_failed`) on a nonzero
    /// nmcli exit. The `autoconnect` field always echoes the requested value
    /// (the intended state is reported even on a failed write).
    pub async fn set_autoconnect(&self, connection_name: &str, enabled: bool) -> Value {
        if connection_name.is_empty() {
            return json!({"autoconnect": false, "name": "", "error": "name_required"});
        }
        let toggle = if enabled { "yes" } else { "no" };
        let out = self
            .runner
            .run(
                &[
                    "nmcli",
                    "connection",
                    "modify",
                    connection_name,
                    "connection.autoconnect",
                    toggle,
                ],
                SYSTEMCTL_TIMEOUT,
            )
            .await;
        if !out.ok() {
            // Pick stderr when it is non-empty (before trimming), else stdout,
            // THEN trim, then fall back to nmcli_failed.
            let source = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            let trimmed = source.trim();
            let err = if trimmed.is_empty() {
                "nmcli_failed".to_string()
            } else {
                trimmed.to_string()
            };
            return json!({"autoconnect": enabled, "name": connection_name, "error": err});
        }
        json!({"autoconnect": enabled, "name": connection_name, "error": null})
    }

    /// Current station status. Mirrors `status`.
    pub async fn status(&self) -> serde_json::Map<String, Value> {
        if self.interface.is_empty() {
            let mut m = serde_json::Map::new();
            for key in ["ssid", "bssid", "signal", "ip", "gateway", "security"] {
                m.insert(key.into(), Value::Null);
            }
            m.insert("connected".into(), json!(false));
            return m;
        }
        let list = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "ACTIVE,SSID,BSSID,SIGNAL,SECURITY",
                    "device",
                    "wifi",
                    "list",
                    "ifname",
                    &self.interface,
                ],
                STATUS_TIMEOUT,
            )
            .await;
        let mut active_ssid: Option<String> = None;
        let mut bssid: Option<String> = None;
        let mut signal_val: Option<i64> = None;
        let mut security: Option<String> = None;
        if list.ok() {
            for row in nmcli::parse_terse(&list.stdout, 5) {
                if row[0].trim() == "yes" {
                    active_ssid = Some(row[1].clone());
                    bssid = Some(row[2].clone());
                    signal_val = row[3].parse::<i64>().ok();
                    security = Some(row[4].clone());
                    break;
                }
            }
        }

        let addr = self
            .runner
            .run(
                &["ip", "-4", "addr", "show", &self.interface],
                STATUS_TIMEOUT,
            )
            .await;
        let ip = if addr.ok() {
            crate::managers::ethernet::parse_inet(&addr.stdout)
        } else {
            None
        };
        let route = self
            .runner
            .run(
                &[
                    "ip",
                    "-4",
                    "route",
                    "show",
                    "default",
                    "dev",
                    &self.interface,
                ],
                STATUS_TIMEOUT,
            )
            .await;
        let gateway = if route.ok() {
            crate::managers::ethernet::parse_default_via(&route.stdout)
        } else {
            None
        };

        let connected = active_ssid.is_some() && ip.is_some();
        let mut m = serde_json::Map::new();
        m.insert("connected".into(), json!(connected));
        m.insert("ssid".into(), json!(active_ssid));
        m.insert("bssid".into(), json!(bssid));
        m.insert("signal".into(), json!(signal_val));
        m.insert("ip".into(), json!(ip));
        m.insert("gateway".into(), json!(gateway));
        m.insert("security".into(), json!(security));
        m
    }
}
