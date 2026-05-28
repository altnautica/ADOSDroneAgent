//! Dashboard data model: extract the fields the dashboard renders from the
//! `/api/v1/setup/status` JSON. Mirrors `_render_dashboard` in the Python CLI
//! so the Rust terminal UI shows the identical information.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct AccessUrl {
    pub label: String,
    pub url: String,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dashboard {
    pub version: String,
    pub device_name: String,
    pub profile: String,
    pub paired: bool,
    pub pairing_code: Option<String>,
    pub access_urls: Vec<AccessUrl>,
    pub steps: Vec<Row>,
    pub status_rows: Vec<Row>,
    pub telemetry: Vec<Row>,
    pub telemetry_empty: bool,
    pub services_running: usize,
    pub services_total: usize,
    pub next_action: String,
}

/// Map a setup-step state to its display label (matches Python `_state_label`).
pub fn state_label(value: &str) -> String {
    match value {
        "complete" => "ready".to_string(),
        "needs_action" => "needs action".to_string(),
        other => other.replace('_', " "),
    }
}

/// Derive the browser viewer URL from a WHEP URL (matches Python
/// `_viewer_url_from_whep`): strip a trailing `/whep`, then end with `/`.
pub fn viewer_url_from_whep(whep_url: Option<&str>) -> Option<String> {
    let whep = whep_url?;
    if whep.is_empty() {
        return None;
    }
    let mut base = whep.trim_end_matches('/').to_string();
    if let Some(stripped) = base.strip_suffix("/whep") {
        base = stripped.to_string();
    }
    Some(format!("{base}/"))
}

fn s(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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
                dash.steps.push(Row {
                    label: s(step, "label", ""),
                    value: state_label(&s(step, "state", "")),
                });
            }
        }

        // Status rows, in the same order the Python dashboard renders them.
        let mavlink = data.get("mavlink").cloned().unwrap_or(Value::Null);
        let video = data.get("video").cloned().unwrap_or(Value::Null);
        let network = data.get("network").cloned().unwrap_or(Value::Null);
        let remote = data.get("remote_access").cloned().unwrap_or(Value::Null);
        let cloud = data.get("cloud_choice").cloned().unwrap_or(Value::Null);

        let mavlink_connected = mavlink
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        dash.status_rows.push(Row {
            label: "MAVLink FC".into(),
            value: if mavlink_connected {
                "connected".into()
            } else {
                "not connected".into()
            },
        });
        if let Some(tcp) = mavlink.get("tcp_url").and_then(Value::as_str) {
            dash.status_rows.push(Row {
                label: "MAVLink TCP".into(),
                value: tcp.to_string(),
            });
        }
        if let Some(ws) = mavlink.get("websocket_url").and_then(Value::as_str) {
            dash.status_rows.push(Row {
                label: "MAVLink WS".into(),
                value: ws.to_string(),
            });
        }

        let video_state = s(&video, "state", "unknown");
        match viewer_url_from_whep(video.get("whep_url").and_then(Value::as_str)) {
            Some(viewer) => dash.status_rows.push(Row {
                label: "Video viewer".into(),
                value: format!("{video_state}  {viewer}"),
            }),
            None => dash.status_rows.push(Row {
                label: "Video".into(),
                value: video_state,
            }),
        }

        dash.status_rows.push(Row {
            label: "Hotspot".into(),
            value: s(&network, "hotspot_ssid", ""),
        });

        // Cloud relay, four cases (matches Python).
        let cloud_paired = cloud
            .get("paired")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let backend_url = s(&cloud, "backend_url", "");
        let cloud_mode = s(&cloud, "mode", "");
        let cloud_relay = if cloud_paired && !backend_url.is_empty() {
            format!("paired ({backend_url})")
        } else if !backend_url.is_empty() && cloud_mode != "local" {
            format!("configured ({backend_url})")
        } else if cloud_mode == "local" {
            "disabled (local mode)".to_string()
        } else {
            "not configured".to_string()
        };
        dash.status_rows.push(Row {
            label: "Cloud relay".into(),
            value: cloud_relay,
        });
        dash.status_rows.push(Row {
            label: "Cloudflare".into(),
            value: s(&remote, "status", "disabled"),
        });

        // Telemetry rows in the Python key order.
        if let Some(telemetry) = data.get("telemetry").and_then(Value::as_object) {
            for key in [
                "mode",
                "armed",
                "battery_remaining",
                "gps_fix",
                "satellites",
                "alt",
            ] {
                if let Some(val) = telemetry.get(key) {
                    dash.telemetry.push(Row {
                        label: title_case(&key.replace('_', " ")),
                        value: value_to_display(val),
                    });
                }
            }
        }
        dash.telemetry_empty = dash.telemetry.is_empty();

        // Services count.
        if let Some(services) = data.get("services").and_then(Value::as_array) {
            dash.services_total = services.len();
            dash.services_running = services
                .iter()
                .filter(|item| item.get("state").and_then(Value::as_str) == Some("running"))
                .count();
        }

        dash.next_action = s(data, "next_action", "");
        dash
    }

    /// The header title line.
    pub fn header_line(&self) -> String {
        let mut line = format!(
            "ADOS Drone Agent  v{}  {} / {}",
            self.version, self.device_name, self.profile
        );
        if self.paired {
            line.push_str("  paired");
        } else if let Some(code) = &self.pairing_code {
            line.push_str(&format!("  code {code}"));
        }
        line
    }
}

/// Render a JSON scalar the way Python's `str()` would for the telemetry grid.
fn value_to_display(v: &Value) -> String {
    match v {
        Value::Bool(b) => {
            // Python str(True) -> "True"
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_label_matches_python() {
        assert_eq!(state_label("complete"), "ready");
        assert_eq!(state_label("needs_action"), "needs action");
        assert_eq!(state_label("in_progress"), "in progress");
        assert_eq!(state_label("pending"), "pending");
    }

    #[test]
    fn viewer_url_strips_whep_suffix() {
        assert_eq!(
            viewer_url_from_whep(Some("http://host:8889/main/whep")).as_deref(),
            Some("http://host:8889/main/")
        );
        assert_eq!(
            viewer_url_from_whep(Some("http://host:8889/main/")).as_deref(),
            Some("http://host:8889/main/")
        );
        assert_eq!(viewer_url_from_whep(None), None);
        assert_eq!(viewer_url_from_whep(Some("")), None);
    }

    #[test]
    fn cloud_relay_local_mode() {
        let data = json!({"cloud_choice": {"mode": "local"}});
        let dash = Dashboard::from_status(&data);
        let relay = dash
            .status_rows
            .iter()
            .find(|r| r.label == "Cloud relay")
            .unwrap();
        assert_eq!(relay.value, "disabled (local mode)");
    }

    #[test]
    fn full_status_extracts_expected_fields() {
        let data = json!({
            "version": "0.46.12",
            "device_name": "ados-e996786c",
            "profile": "ground_station",
            "paired": false,
            "pairing_code": "WM325P",
            "access_urls": [
                {"label": "Setup", "url": "http://x:8080/setup", "primary": true}
            ],
            "steps": [{"label": "Profile", "state": "complete"}],
            "mavlink": {"connected": false, "tcp_url": "tcp://x:5760"},
            "video": {"state": "running", "whep_url": "http://x:8889/main/whep"},
            "network": {"hotspot_ssid": "ADOS-AP"},
            "remote_access": {"status": "disabled"},
            "cloud_choice": {"mode": "local"},
            "telemetry": {"mode": "STABILIZE", "armed": false, "satellites": 14},
            "services": [{"state": "running"}, {"state": "running"}, {"state": "stopped"}],
            "next_action": "Open Mission Control"
        });
        let dash = Dashboard::from_status(&data);

        assert_eq!(
            dash.header_line(),
            "ADOS Drone Agent  v0.46.12  ados-e996786c / ground_station  code WM325P"
        );
        assert_eq!(dash.access_urls.len(), 1);
        assert!(dash.access_urls[0].primary);
        assert_eq!(dash.steps[0].value, "ready");

        let labels: Vec<&str> = dash.status_rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "MAVLink FC",
                "MAVLink TCP",
                "Video viewer",
                "Hotspot",
                "Cloud relay",
                "Cloudflare"
            ]
        );
        let video = dash
            .status_rows
            .iter()
            .find(|r| r.label == "Video viewer")
            .unwrap();
        assert_eq!(video.value, "running  http://x:8889/main/");

        // Telemetry: title-cased labels, Python-style bool, key order preserved.
        let tel: Vec<(&str, &str)> = dash
            .telemetry
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect();
        assert_eq!(
            tel,
            vec![
                ("Mode", "STABILIZE"),
                ("Armed", "False"),
                ("Satellites", "14")
            ]
        );
        assert_eq!((dash.services_running, dash.services_total), (2, 3));
        assert_eq!(dash.next_action, "Open Mission Control");
    }

    #[test]
    fn empty_telemetry_flagged() {
        let dash = Dashboard::from_status(&json!({}));
        assert!(dash.telemetry_empty);
        assert_eq!(dash.version, "?");
    }
}
