//! Building a Dashboard from the agent status document.

use super::*;

impl Dashboard {
    /// Build the dashboard model from a `/api/v1/setup/status` response.
    pub fn from_status(data: &Value) -> Self {
        let mut dash = Dashboard {
            version: s(data, "version", "?"),
            device_name: s(data, "device_name", "?"),
            profile: s(data, "profile", "?"),
            paired: data.get("paired").and_then(Value::as_bool).unwrap_or(false),
            pairing_code: data
                .get("pairing_code")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Dashboard::default()
        };

        // Access URLs (first 10).
        if let Some(urls) = data.get("access_urls").and_then(Value::as_array) {
            for item in urls.iter().take(10) {
                dash.access_urls.push(AccessUrl {
                    label: s(item, "label", "URL"),
                    url: s(item, "url", ""),
                    kind: s(item, "kind", ""),
                    primary: item
                        .get("primary")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
        }

        // Setup steps.
        if let Some(steps) = data.get("steps").and_then(Value::as_array) {
            for step in steps {
                dash.steps.push(Step {
                    label: s(step, "label", ""),
                    state: s(step, "state", ""),
                    detail: s(step, "detail", ""),
                });
            }
        }
        dash.has_steps = !dash.steps.is_empty();
        dash.steps_all_complete =
            dash.has_steps && dash.steps.iter().all(|step| step.state == "complete");

        // MAVLink.
        let mavlink = data.get("mavlink").cloned().unwrap_or(Value::Null);
        dash.mavlink_connected = mavlink
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        dash.mavlink_tcp_url = opt_str(&mavlink, "tcp_url");
        dash.mavlink_ws_url = opt_str(&mavlink, "websocket_url");
        dash.mavlink_public_ws_url = opt_str(&mavlink, "public_websocket_url");
        dash.fc_port = opt_str(&mavlink, "port");
        dash.fc_baud = mavlink
            .get("baud")
            .and_then(Value::as_i64)
            .filter(|b| *b > 0);

        // Video.
        let video = data.get("video").cloned().unwrap_or(Value::Null);
        dash.video_state = s(&video, "state", "unknown");
        dash.video_viewer = viewer_url_from_whep(video.get("whep_url").and_then(Value::as_str));

        // Network + cloud relay + remote access.
        let network = data.get("network").cloned().unwrap_or(Value::Null);
        let remote = data.get("remote_access").cloned().unwrap_or(Value::Null);
        let cloud = data.get("cloud_choice").cloned().unwrap_or(Value::Null);

        // Only show the hotspot SSID when the hotspot is actually enabled: the
        // config SSID is always populated, so reading it alone shows the row as
        // broadcasting even when the hotspot is off.
        let hotspot_on = network
            .get("hotspot_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        dash.hotspot = if hotspot_on {
            s(&network, "hotspot_ssid", "")
        } else {
            String::new()
        };
        dash.hotspot_passphrase = if hotspot_on {
            read_ap_passphrase()
        } else {
            String::new()
        };
        dash.remote_status = s(&remote, "status", "disabled");

        dash.cloud_paired = cloud
            .get("paired")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let backend_url = s(&cloud, "backend_url", "");
        dash.cloud_mode = s(&cloud, "mode", "");
        dash.cloud_configured = !backend_url.is_empty() && dash.cloud_mode != "local";
        dash.cloud_relay = if dash.cloud_paired && !backend_url.is_empty() {
            format!("paired ({backend_url})")
        } else if dash.cloud_configured {
            format!("configured ({backend_url})")
        } else if dash.cloud_mode == "local" {
            "disabled (local mode)".to_string()
        } else {
            "not configured".to_string()
        };

        // Flight telemetry, when the setup status carries a snapshot (the
        // consolidated status's copy is merged by `merge_telemetry`).
        if let Some(tel) = data.get("telemetry").filter(|t| t.is_object()) {
            dash.read_telemetry(tel);
        }

        dash.next_action = s(data, "next_action", "");
        dash.hardware_counts = hardware_counts(data);
        dash.links = dash.reach_links();
        dash
    }
}

impl Dashboard {
    /// Merge the router telemetry snapshot from `/api/status/full`. The native
    /// setup status does not embed telemetry, so this is where mode, arm state,
    /// battery, GPS and altitude come from.
    pub fn merge_telemetry(&mut self, full: &Value) {
        if let Some(tel) = full.get("telemetry").filter(|t| t.is_object()) {
            self.read_telemetry(tel);
        }
    }

    /// Read a router snapshot. The snapshot is nested (battery / gps /
    /// position sub-objects); mode + armed are the only flat top-level keys.
    /// It is read only when the FC link is live, so the empty snapshot's zeros
    /// (and the -1 "unknown" battery) never render as a real reading on a
    /// disconnected drone.
    fn read_telemetry(&mut self, tel: &Value) {
        if !self.mavlink_connected {
            return;
        }
        self.mode = tel
            .get("mode")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        self.armed = tel.get("armed").and_then(Value::as_bool);
        // battery.remaining is a percent, or -1 until the FC reports one.
        self.battery = tel
            .get("battery")
            .and_then(|b| b.get("remaining"))
            .and_then(Value::as_f64)
            .filter(|v| *v >= 0.0);
        // gps.fix_type is a MAVLink GPS_FIX_TYPE int rendered as a label.
        self.gps_fix = tel
            .get("gps")
            .and_then(|g| g.get("fix_type"))
            .and_then(Value::as_i64)
            .map(fix_label);
        self.satellites = tel
            .get("gps")
            .and_then(|g| g.get("satellites"))
            .and_then(Value::as_i64);
        // Altitude relative to home is the operator-meaningful number.
        self.alt = tel
            .get("position")
            .and_then(|p| p.get("alt_rel"))
            .and_then(Value::as_f64);
    }
}

/// Item counts per hardware-check state, in severity order, from
/// `hardware_check.items`. `None` when the check has not reported items.
fn hardware_counts(data: &Value) -> Option<Vec<(String, usize)>> {
    let items = data.get("hardware_check")?.get("items")?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let mut counts: Vec<(String, usize)> = ["ok", "warning", "missing", "checking", "unknown"]
        .iter()
        .map(|s| (s.to_string(), 0))
        .collect();
    for item in items {
        let state = item
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match counts.iter_mut().find(|(s, _)| s == state) {
            Some((_, n)) => *n += 1,
            None => counts.push((state.to_string(), 1)),
        }
    }
    counts.retain(|(_, n)| *n > 0);
    Some(counts)
}
