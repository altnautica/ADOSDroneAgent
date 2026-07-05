//! Dashboard data model: extract the fields the terminal dashboard renders from
//! the `/api/v1/setup/status` JSON.
//!
//! Everything the UI shows is a typed field pulled straight out of the payload,
//! so the screen only ever renders values that are actually present. No metric
//! is synthesised. The health verdict and the reach-ordering live here so they
//! can be unit-tested without a terminal.

use serde_json::Value;

/// A single advertised way to reach the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessUrl {
    pub label: String,
    pub url: String,
    pub primary: bool,
}

/// A console reach endpoint: the agent's web UI as a bare `host:port`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReachHost {
    pub host_port: String,
    pub primary: bool,
    pub loopback: bool,
}

/// A setup-wizard step with its raw state (e.g. `complete`, `needs_action`).
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub label: String,
    pub state: String,
}

/// The overall one-word health verdict, computed only from verified inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded,
    Setup,
}

impl Health {
    /// The status glyph shown before the word.
    pub fn dot(self) -> &'static str {
        match self {
            Health::Healthy => "●",
            Health::Degraded => "▲",
            Health::Setup => "●",
        }
    }

    /// The verdict word.
    pub fn label(self) -> &'static str {
        match self {
            Health::Healthy => "HEALTHY",
            Health::Degraded => "DEGRADED",
            Health::Setup => "SETUP",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dashboard {
    pub version: String,
    pub device_name: String,
    pub profile: String,
    pub paired: bool,
    pub pairing_code: Option<String>,
    pub access_urls: Vec<AccessUrl>,

    pub steps: Vec<Step>,
    pub has_steps: bool,
    pub steps_all_complete: bool,

    pub mavlink_connected: bool,

    pub video_state: String,
    pub video_viewer: Option<String>,

    pub cloud_relay: String,
    pub cloud_mode: String,
    pub cloud_paired: bool,
    pub cloud_configured: bool,
    pub remote_status: String,
    pub hotspot: String,

    pub mode: Option<String>,
    pub armed: Option<bool>,
    pub battery: Option<f64>,
    pub gps_fix: Option<String>,
    pub satellites: Option<i64>,
    pub alt: Option<f64>,

    pub services_running: usize,
    pub services_total: usize,
    pub next_action: String,
}

/// Map a setup-step state to its display label.
pub fn state_label(value: &str) -> String {
    match value {
        "complete" => "ready".to_string(),
        "needs_action" => "needs action".to_string(),
        other => other.replace('_', " "),
    }
}

/// Derive the browser viewer URL from a WHEP URL: strip a trailing `/whep`,
/// then end with `/`.
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

/// The lowercased host of a URL, with the scheme, path, and port stripped.
fn url_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        // Bracketed IPv6 literal, e.g. `[::1]:8080`.
        rest.split(']').next().unwrap_or(rest)
    } else {
        host_port
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(host_port)
    };
    host.to_ascii_lowercase()
}

/// The `host:port` authority of a URL, with the scheme and path stripped.
fn url_host_port(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
        .to_string()
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

fn is_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Reach priority for a URL, lowest first: mDNS `.local` (0), LAN IP (1),
/// other hostname / tunnel (2), loopback (3). The operator is almost always
/// on another machine, so `localhost` is the least useful and sorts last.
pub fn reach_rank(url: &str) -> u8 {
    let host = url_host(url);
    if is_loopback_host(&host) {
        3
    } else if host.ends_with(".local") {
        0
    } else if is_ipv4(&host) {
        1
    } else {
        2
    }
}

/// True when the URL points at the local loopback (useless over SSH).
pub fn is_loopback_url(url: &str) -> bool {
    is_loopback_host(&url_host(url))
}

fn s(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

/// A MAVLink `GPS_FIX_TYPE` integer as a short human label.
pub fn fix_label(fix_type: i64) -> String {
    match fix_type {
        0 => "no GPS",
        1 => "no fix",
        2 => "2D",
        3 => "3D",
        4 => "DGPS",
        5 => "RTK float",
        6 => "RTK fixed",
        _ => "unknown",
    }
    .to_string()
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
                dash.steps.push(Step {
                    label: s(step, "label", ""),
                    state: s(step, "state", ""),
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

        // Flight telemetry. The router snapshot is nested (battery / gps /
        // position sub-objects); mode + armed are the only flat top-level keys.
        // It is read only when the FC link is live, so the empty snapshot's
        // zeros (and the -1 "unknown" battery) never render as a real reading on
        // a disconnected drone.
        if dash.mavlink_connected {
            let tel = data.get("telemetry").cloned().unwrap_or(Value::Null);
            dash.mode = tel
                .get("mode")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string);
            dash.armed = tel.get("armed").and_then(Value::as_bool);
            // battery.remaining is a percent, or -1 until the FC reports one.
            dash.battery = tel
                .get("battery")
                .and_then(|b| b.get("remaining"))
                .and_then(Value::as_f64)
                .filter(|v| *v >= 0.0);
            // gps.fix_type is a MAVLink GPS_FIX_TYPE int rendered as a label.
            dash.gps_fix = tel
                .get("gps")
                .and_then(|g| g.get("fix_type"))
                .and_then(Value::as_i64)
                .map(fix_label);
            dash.satellites = tel
                .get("gps")
                .and_then(|g| g.get("satellites"))
                .and_then(Value::as_i64);
            // Altitude relative to home is the operator-meaningful number.
            dash.alt = tel
                .get("position")
                .and_then(|p| p.get("alt_rel"))
                .and_then(Value::as_f64);
        }

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

    /// The console addresses to open in a browser (the agent's own web UI),
    /// best first: mDNS `.local`, then LAN IPs, then other hosts, with loopback
    /// last and shown only when no routable address exists. Filtered to the
    /// setup/console pages (not video / MAVLink / Mission Control) and reduced to
    /// a de-duplicated bare `host:port`, so the narrow panel never has to wrap a
    /// long URL.
    pub fn console_reach(&self) -> Vec<ReachHost> {
        let mut sorted = self.access_urls.clone();
        sorted.sort_by_key(|item| reach_rank(&item.url));
        let mut out: Vec<ReachHost> = Vec::new();
        for item in &sorted {
            if !item.url.contains("/setup") {
                continue; // the agent console only, not video / MAVLink / Mission Control
            }
            let host_port = url_host_port(&item.url);
            if host_port.is_empty() || out.iter().any(|r| r.host_port == host_port) {
                continue;
            }
            out.push(ReachHost {
                host_port,
                primary: item.primary,
                loopback: is_loopback_url(&item.url),
            });
        }
        // A remote operator cannot reach loopback: drop it once anything else exists.
        if out.iter().any(|r| !r.loopback) {
            out.retain(|r| !r.loopback);
        }
        out
    }

    /// The overall health verdict, from verified inputs only.
    pub fn health(&self) -> Health {
        let services_all_running =
            self.services_total > 0 && self.services_running == self.services_total;
        let services_failed =
            self.services_total > 0 && self.services_running < self.services_total;
        let is_ground_station = self.profile == "ground_station";
        let mavlink_ok = self.mavlink_connected || is_ground_station;
        let configured = self.paired || (self.has_steps && self.steps_all_complete);

        if services_failed {
            Health::Degraded
        } else if configured && !mavlink_ok {
            // A set-up drone that has lost its flight controller.
            Health::Degraded
        } else if services_all_running && configured && mavlink_ok {
            Health::Healthy
        } else {
            Health::Setup
        }
    }

    /// The pairing / link mode used for the verdict sub-line.
    fn link_mode(&self) -> &'static str {
        let remote = self.remote_status.as_str();
        if !matches!(remote, "" | "disabled" | "off" | "stopped" | "unknown") {
            "cloudflare"
        } else if self.cloud_paired || self.cloud_configured {
            "cloud relay"
        } else {
            "local mode"
        }
    }

    /// The dim sub-line under the verdict, e.g. `paired · local mode`.
    pub fn status_summary(&self) -> String {
        let pairing = if self.paired {
            "paired".to_string()
        } else if let Some(code) = &self.pairing_code {
            format!("code {code}")
        } else {
            "not paired".to_string()
        };
        format!("{pairing} · {}", self.link_mode())
    }

    /// The device-and-profile identity shown in the header, e.g.
    /// `ados-e996786c · ground_station`. The word-mark and version are styled
    /// separately by the renderer.
    pub fn ident(&self) -> String {
        format!("{} · {}", self.device_name, self.profile)
    }
}

/// How many polled samples the trend sparklines retain.
pub const HISTORY_CAP: usize = 30;

/// A small ring buffer of verified telemetry values for the trend sparklines.
/// A value is recorded only when it is actually present in the payload, so the
/// trend is always real data.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub battery: Vec<f64>,
    pub alt: Vec<f64>,
}

fn push_cap(buffer: &mut Vec<f64>, value: f64) {
    buffer.push(value);
    if buffer.len() > HISTORY_CAP {
        let overflow = buffer.len() - HISTORY_CAP;
        buffer.drain(0..overflow);
    }
}

impl History {
    /// Record the present telemetry values from one poll.
    pub fn record(&mut self, dash: &Dashboard) {
        if let Some(battery) = dash.battery {
            push_cap(&mut self.battery, battery);
        }
        if let Some(alt) = dash.alt {
            push_cap(&mut self.alt, alt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_label_matches_states() {
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
    fn ident_is_device_and_profile() {
        let dash = Dashboard {
            device_name: "ados-e996786c".into(),
            profile: "ground_station".into(),
            ..Dashboard::default()
        };
        assert_eq!(dash.ident(), "ados-e996786c · ground_station");
    }

    #[test]
    fn reach_rank_classifies_hosts() {
        assert_eq!(reach_rank("http://ados-abc.local:8080/setup"), 0);
        assert_eq!(reach_rank("http://192.168.1.5:8080/setup"), 1);
        assert_eq!(reach_rank("https://tunnel.example.com/setup"), 2);
        assert_eq!(reach_rank("http://localhost:8080/setup"), 3);
        assert_eq!(reach_rank("http://127.0.0.1:8080/setup"), 3);
        assert_eq!(reach_rank("http://[::1]:8080/setup"), 3);
    }

    #[test]
    fn is_loopback_url_detects_loopback() {
        assert!(is_loopback_url("http://localhost:8080/"));
        assert!(is_loopback_url("http://127.0.0.1:8080/"));
        assert!(!is_loopback_url("http://ados-abc.local:8080/"));
        assert!(!is_loopback_url("http://192.168.1.5:8080/"));
    }

    #[test]
    fn console_reach_orders_dedupes_and_drops_loopback_and_non_console() {
        let data = json!({
            "access_urls": [
                {"label": "local", "url": "http://localhost:8080/setup"},
                {"label": "lan", "url": "http://192.168.1.5:8080/setup"},
                {"label": "mdns", "url": "http://ados-abc.local:8080/setup", "primary": true},
                {"label": "lan2", "url": "http://10.0.0.9:8080/setup"},
                {"label": "lan-dup", "url": "http://192.168.1.5:8080/setup"},
                {"label": "video", "url": "http://ados-abc.local:8889/main/"},
                {"label": "mc", "url": "http://localhost:4000"}
            ]
        });
        let dash = Dashboard::from_status(&data);
        let hosts: Vec<String> = dash
            .console_reach()
            .into_iter()
            .map(|r| r.host_port)
            .collect();
        // mDNS first, then LAN IPs (deduped, producer order); loopback + non-/setup dropped.
        assert_eq!(
            hosts,
            vec!["ados-abc.local:8080", "192.168.1.5:8080", "10.0.0.9:8080"]
        );
        // The mDNS entry is the primary and none of the survivors are loopback.
        let reach = dash.console_reach();
        assert!(reach[0].primary);
        assert!(reach.iter().all(|r| !r.loopback));
    }

    #[test]
    fn console_reach_keeps_loopback_only_when_nothing_else() {
        let data = json!({
            "access_urls": [{"label": "local", "url": "http://localhost:8080/setup", "primary": true}]
        });
        let dash = Dashboard::from_status(&data);
        let reach = dash.console_reach();
        assert_eq!(reach.len(), 1);
        assert_eq!(reach[0].host_port, "localhost:8080");
        assert!(reach[0].loopback);
    }

    #[test]
    fn health_healthy_when_configured_connected_and_all_running() {
        let data = json!({
            "profile": "drone",
            "paired": true,
            "mavlink": {"connected": true},
            "services": [{"state": "running"}, {"state": "running"}],
            "steps": [{"label": "Profile", "state": "complete"}]
        });
        assert_eq!(Dashboard::from_status(&data).health(), Health::Healthy);
    }

    #[test]
    fn health_degraded_when_a_service_is_down() {
        let data = json!({
            "profile": "drone",
            "paired": true,
            "mavlink": {"connected": true},
            "services": [{"state": "running"}, {"state": "stopped"}],
            "steps": [{"label": "Profile", "state": "complete"}]
        });
        assert_eq!(Dashboard::from_status(&data).health(), Health::Degraded);
    }

    #[test]
    fn health_degraded_when_configured_drone_loses_fc() {
        let data = json!({
            "profile": "drone",
            "paired": true,
            "mavlink": {"connected": false},
            "services": [{"state": "running"}]
        });
        assert_eq!(Dashboard::from_status(&data).health(), Health::Degraded);
    }

    #[test]
    fn health_setup_when_unpaired_and_steps_incomplete() {
        let data = json!({
            "profile": "drone",
            "paired": false,
            "mavlink": {"connected": false},
            "services": [{"state": "running"}],
            "steps": [{"label": "Profile", "state": "pending"}]
        });
        assert_eq!(Dashboard::from_status(&data).health(), Health::Setup);
    }

    #[test]
    fn health_healthy_for_ground_station_without_fc() {
        let data = json!({
            "profile": "ground_station",
            "paired": true,
            "mavlink": {"connected": false},
            "services": [{"state": "running"}]
        });
        assert_eq!(Dashboard::from_status(&data).health(), Health::Healthy);
    }

    #[test]
    fn status_summary_combines_pairing_and_link() {
        let paired_local = Dashboard {
            paired: true,
            cloud_mode: "local".into(),
            ..Dashboard::default()
        };
        assert_eq!(paired_local.status_summary(), "paired · local mode");

        let code_relay = Dashboard {
            paired: false,
            pairing_code: Some("AB12".into()),
            cloud_configured: true,
            ..Dashboard::default()
        };
        assert_eq!(code_relay.status_summary(), "code AB12 · cloud relay");
    }

    #[test]
    fn cloud_relay_local_mode() {
        let data = json!({"cloud_choice": {"mode": "local"}});
        let dash = Dashboard::from_status(&data);
        assert_eq!(dash.cloud_relay, "disabled (local mode)");
    }

    #[test]
    fn full_status_extracts_typed_fields() {
        let data = json!({
            "version": "0.46.12",
            "device_name": "ados-e996786c",
            "profile": "drone",
            "paired": false,
            "pairing_code": "WM325P",
            "access_urls": [
                {"label": "Setup", "url": "http://ados-x.local:8080/setup", "primary": true}
            ],
            "steps": [{"label": "Profile", "state": "complete"}],
            "mavlink": {"connected": true, "tcp_url": "tcp://x:5760"},
            "video": {"state": "running", "whep_url": "http://x:8889/main/whep"},
            "network": {"hotspot_enabled": true, "hotspot_ssid": "ADOS-AP"},
            "remote_access": {"status": "disabled"},
            "cloud_choice": {"mode": "local"},
            "telemetry": {
                "mode": "STABILIZE", "armed": false,
                "battery": {"remaining": 82},
                "gps": {"fix_type": 3, "satellites": 14},
                "position": {"alt_rel": 12.5}
            },
            "services": [{"state": "running"}, {"state": "running"}, {"state": "stopped"}],
            "next_action": "Open Mission Control"
        });
        let dash = Dashboard::from_status(&data);

        assert_eq!(dash.version, "0.46.12");
        assert_eq!(dash.device_name, "ados-e996786c");
        assert_eq!(dash.pairing_code.as_deref(), Some("WM325P"));
        assert!(dash.access_urls[0].primary);
        assert!(dash.steps_all_complete);
        assert!(dash.mavlink_connected);
        assert_eq!(dash.video_state, "running");
        assert_eq!(dash.video_viewer.as_deref(), Some("http://x:8889/main/"));
        assert_eq!(dash.hotspot, "ADOS-AP");
        assert_eq!(dash.mode.as_deref(), Some("STABILIZE"));
        assert_eq!(dash.armed, Some(false));
        assert_eq!(dash.battery, Some(82.0));
        assert_eq!(dash.gps_fix.as_deref(), Some("3D"));
        assert_eq!(dash.satellites, Some(14));
        assert_eq!(dash.alt, Some(12.5));
        assert_eq!((dash.services_running, dash.services_total), (2, 3));
        assert_eq!(dash.next_action, "Open Mission Control");
    }

    #[test]
    fn empty_status_has_no_telemetry() {
        let dash = Dashboard::from_status(&json!({}));
        assert!(dash.mode.is_none());
        assert!(dash.battery.is_none());
        assert!(dash.alt.is_none());
        assert_eq!(dash.version, "?");
    }

    #[test]
    fn fix_label_maps_gps_fix_types() {
        assert_eq!(fix_label(0), "no GPS");
        assert_eq!(fix_label(1), "no fix");
        assert_eq!(fix_label(3), "3D");
        assert_eq!(fix_label(6), "RTK fixed");
        assert_eq!(fix_label(99), "unknown");
    }

    #[test]
    fn flight_telemetry_hidden_when_fc_disconnected() {
        // The empty snapshot carries all-zero nested telemetry even with the FC
        // down; none of it may render, or a disconnected drone reads live.
        let data = json!({
            "profile": "drone",
            "mavlink": {"connected": false},
            "telemetry": {
                "mode": "", "armed": false,
                "battery": {"remaining": -1},
                "gps": {"fix_type": 0, "satellites": 0},
                "position": {"alt_rel": 0.0}
            }
        });
        let dash = Dashboard::from_status(&data);
        assert!(dash.mode.is_none());
        assert!(dash.battery.is_none());
        assert!(dash.gps_fix.is_none());
        assert!(dash.satellites.is_none());
        assert!(dash.alt.is_none());
    }

    #[test]
    fn battery_unknown_when_negative() {
        let data = json!({
            "mavlink": {"connected": true},
            "telemetry": {"battery": {"remaining": -1}}
        });
        assert!(Dashboard::from_status(&data).battery.is_none());
    }

    #[test]
    fn hotspot_hidden_when_disabled() {
        let off = json!({"network": {"hotspot_enabled": false, "hotspot_ssid": "ADOS-AP"}});
        assert_eq!(Dashboard::from_status(&off).hotspot, "");
        let on = json!({"network": {"hotspot_enabled": true, "hotspot_ssid": "ADOS-AP"}});
        assert_eq!(Dashboard::from_status(&on).hotspot, "ADOS-AP");
    }

    #[test]
    fn history_records_present_values_and_caps() {
        let mut history = History::default();
        for i in 0..(HISTORY_CAP + 5) {
            let dash = Dashboard {
                battery: Some(i as f64),
                alt: None,
                ..Dashboard::default()
            };
            history.record(&dash);
        }
        // Battery capped at HISTORY_CAP, oldest dropped; alt never recorded.
        assert_eq!(history.battery.len(), HISTORY_CAP);
        assert_eq!(*history.battery.first().unwrap(), 5.0);
        assert!(history.alt.is_empty());
    }
}
