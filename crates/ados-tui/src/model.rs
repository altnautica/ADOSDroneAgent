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
    /// The kind of endpoint: `setup`, `api`, `mission_control`, `video`,
    /// `mavlink`, or `cloud`.
    pub kind: String,
    pub primary: bool,
}

/// One openable link in the Links panel: a full URL, its primacy, and whether
/// it points at the (remotely useless) loopback.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub url: String,
    pub primary: bool,
    pub loopback: bool,
}

/// A titled group of links, e.g. `Console` or `Ground control`.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkGroup {
    pub title: &'static str,
    pub rows: Vec<LinkRow>,
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

/// The flight-controller link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcLink {
    /// Transport open and a fresh HEARTBEAT — the gated "connected" truth.
    Connected,
    /// Transport open but no HEARTBEAT decoded (e.g. an MSP FC, or wrong baud).
    PortOpen,
    /// No FC link.
    Down,
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
    pub mavlink_tcp_url: Option<String>,
    pub mavlink_ws_url: Option<String>,
    pub mavlink_public_ws_url: Option<String>,
    pub fc_port: Option<String>,
    pub fc_baud: Option<i64>,
    // Richer FC-link truth, merged from the native `/api/status` route when it
    // is reachable (absent → the `mavlink_connected` gated boolean stands).
    pub transport_open: Option<bool>,
    pub mavlink_alive: Option<bool>,
    pub fc_link_hint: Option<String>,

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

/// A reachable host used to project per-host service URLs (mDNS name or LAN IP).
struct HostRef {
    host: String,
    primary: bool,
}

/// De-duplicate rows by URL (first wins) and drop loopback rows once any
/// routable row exists.
fn dedup_drop_loopback(rows: Vec<LinkRow>) -> Vec<LinkRow> {
    let mut out: Vec<LinkRow> = Vec::new();
    for row in rows {
        if !out.iter().any(|r| r.url == row.url) {
            out.push(row);
        }
    }
    if out.iter().any(|r| !r.loopback) {
        out.retain(|r| !r.loopback);
    }
    out
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

/// A non-empty string field as `Option<String>`.
fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The port number in a URL authority, e.g. `8765` from `ws://host:8765/`.
fn port_of(url: &str) -> Option<u16> {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let after = authority.rsplit(']').next().unwrap_or(authority); // past an IPv6 literal
    after.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

/// Replace the host in a URL, preserving the scheme, port, and path. Used to
/// project a service URL (advertised on one best host) onto every reachable
/// host so the operator sees both the mDNS name and the LAN IP.
fn swap_host(url: &str, host: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Keep the `:port` suffix of the old authority (skip an IPv6 literal).
    let port = authority
        .rsplit(']')
        .next()
        .unwrap_or(authority)
        .rsplit_once(':')
        .map(|(_, p)| format!(":{p}"))
        .unwrap_or_default();
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{port}{tail}"),
        None => format!("{host}{port}{tail}"),
    }
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

    /// Every advertised link, grouped for the Links panel: Console (the setup
    /// webapp), Ground control (MAVLink tcp + ws), Video, Mission Control, and
    /// Remote. Each link is a full URL. The MAVLink and video endpoints, which
    /// the agent advertises on one best host, are projected onto every reachable
    /// LAN host so the operator sees both the mDNS name and the LAN IP. Loopback
    /// is dropped from a group once it has any routable link.
    pub fn reach_links(&self) -> Vec<LinkGroup> {
        let hosts = self.lan_hosts();
        let mut out: Vec<LinkGroup> = Vec::new();

        // Console: the setup webapp, already advertised per host.
        out.push(LinkGroup {
            title: "Console",
            rows: self.rows_for(&["setup"]),
        });

        // Ground control: a tcp + ws endpoint on every reachable LAN host (both
        // are bound on 0.0.0.0), plus any advertised tunnel endpoint.
        let tcp_port = self
            .mavlink_tcp_url
            .as_deref()
            .and_then(port_of)
            .unwrap_or(5760);
        let ws_port = self
            .mavlink_ws_url
            .as_deref()
            .and_then(port_of)
            .unwrap_or(8765);
        let mut gc: Vec<LinkRow> = Vec::new();
        if self.mavlink_tcp_url.is_some() || self.mavlink_ws_url.is_some() {
            for h in &hosts {
                gc.push(LinkRow {
                    url: format!("tcp://{}:{tcp_port}", h.host),
                    primary: h.primary,
                    loopback: false,
                });
                gc.push(LinkRow {
                    url: format!("ws://{}:{ws_port}/", h.host),
                    primary: false,
                    loopback: false,
                });
            }
        }
        if let Some(pub_ws) = &self.mavlink_public_ws_url {
            gc.push(LinkRow {
                url: pub_ws.clone(),
                primary: false,
                loopback: false,
            });
        }
        gc.extend(self.rows_for(&["mavlink"])); // e.g. the tunnel MAVLink WS
        out.push(LinkGroup {
            title: "Ground control",
            rows: dedup_drop_loopback(gc),
        });

        // Video: project the advertised viewer URL (or the WHEP viewer) onto
        // every LAN host, plus any advertised tunnel viewer.
        let mut vid: Vec<LinkRow> = Vec::new();
        if let Some(tmpl) = self
            .best_url(&["video"])
            .or_else(|| self.video_viewer.clone())
        {
            for h in &hosts {
                vid.push(LinkRow {
                    url: swap_host(&tmpl, &h.host),
                    primary: h.primary,
                    loopback: false,
                });
            }
        }
        vid.extend(self.rows_for(&["video"]));
        out.push(LinkGroup {
            title: "Video",
            rows: dedup_drop_loopback(vid),
        });

        // Mission Control + Remote: as advertised.
        out.push(LinkGroup {
            title: "Mission Control",
            rows: self.rows_for(&["mission_control"]),
        });
        out.push(LinkGroup {
            title: "Remote",
            rows: self.rows_for(&["cloud"]),
        });

        out.retain(|g| !g.rows.is_empty());
        out
    }

    /// The reachable LAN hosts (mDNS `.local` first, then LAN IPs) taken from
    /// the advertised access URLs. Loopback and non-LAN (tunnel) hosts are
    /// excluded — those ride their own advertised entries.
    fn lan_hosts(&self) -> Vec<HostRef> {
        let mut sorted = self.access_urls.clone();
        sorted.sort_by_key(|item| reach_rank(&item.url));
        let mut out: Vec<HostRef> = Vec::new();
        for item in &sorted {
            if reach_rank(&item.url) > 1 {
                continue; // 0 = .local, 1 = LAN IP; skip tunnel/other/loopback
            }
            let host = url_host(&item.url);
            if host.is_empty() || out.iter().any(|h| h.host == host) {
                continue;
            }
            out.push(HostRef {
                host,
                primary: item.primary,
            });
        }
        out
    }

    /// The rows for the given kinds, ordered best-first, deduped, loopback
    /// dropped once a routable link exists.
    fn rows_for(&self, kinds: &[&str]) -> Vec<LinkRow> {
        let mut sorted = self.access_urls.clone();
        sorted.sort_by_key(|item| reach_rank(&item.url));
        let rows = sorted
            .iter()
            .filter(|item| kinds.contains(&item.kind.as_str()) && !item.url.is_empty())
            .map(|item| LinkRow {
                url: item.url.clone(),
                primary: item.primary,
                loopback: is_loopback_url(&item.url),
            })
            .collect();
        dedup_drop_loopback(rows)
    }

    /// The best (most reachable, non-loopback) advertised URL of the given kinds.
    fn best_url(&self, kinds: &[&str]) -> Option<String> {
        self.rows_for(kinds).into_iter().next().map(|r| r.url)
    }

    /// Merge the richer FC-link truth from the native `/api/status` route. The
    /// setup-status `mavlink.connected` is the gated boolean; the transport-open-
    /// but-silent distinction lives here. Best-effort: called only when the
    /// route was reachable this poll.
    pub fn merge_fc_status(&mut self, status: &Value) {
        self.transport_open = status.get("transportOpen").and_then(Value::as_bool);
        self.mavlink_alive = status.get("mavlinkAlive").and_then(Value::as_bool);
        self.fc_link_hint = status
            .get("fcLinkHint")
            .and_then(Value::as_str)
            .filter(|h| !h.is_empty() && *h != "none")
            .map(str::to_string);
        // Fall back to /api/status port/baud only when setup-status lacked them.
        if self.fc_port.is_none() {
            self.fc_port = opt_str(status, "fc_port");
        }
        if self.fc_baud.is_none() {
            self.fc_baud = status
                .get("fc_baud")
                .and_then(Value::as_i64)
                .filter(|b| *b > 0);
        }
    }

    /// The FC-link state: the gated `connected` truth, refined by the
    /// `/api/status` transport/heartbeat split into a "port open · no MAVLink"
    /// case when that detail is available.
    pub fn fc_link(&self) -> FcLink {
        if self.mavlink_connected {
            FcLink::Connected
        } else if self.transport_open == Some(true) && self.mavlink_alive == Some(false) {
            FcLink::PortOpen
        } else {
            FcLink::Down
        }
    }

    /// A short link hint (e.g. `msp detected`), when the router reports one.
    pub fn fc_hint(&self) -> Option<String> {
        self.fc_link_hint.as_ref().map(|h| h.replace('_', " "))
    }

    /// The FC serial detail, e.g. `ttyS0 · 921600`, when a port is known.
    pub fn fc_endpoint(&self) -> Option<String> {
        let port = self.fc_port.as_ref()?;
        Some(match self.fc_baud {
            Some(baud) => format!("{port} · {baud}"),
            None => port.clone(),
        })
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
    fn reach_links_groups_all_kinds_full_url_mdns_and_ip() {
        let data = json!({
            "access_urls": [
                {"kind": "setup", "label": "mDNS setup", "url": "http://ados-x.local:8080/setup", "primary": true},
                {"kind": "setup", "label": "LAN setup", "url": "http://192.168.1.5:8080/setup"},
                {"kind": "setup", "label": "local", "url": "http://localhost:8080/setup"},
                {"kind": "video", "label": "Local video viewer", "url": "http://ados-x.local:8889/main/"},
                {"kind": "mavlink", "label": "MAVLink WebSocket", "url": "ws://ados-x.local:8765/"},
                {"kind": "mission_control", "label": "Mission Control", "url": "https://command.example.com"},
                {"kind": "cloud", "label": "Remote access", "url": "https://tunnel.example.com/setup"}
            ],
            "mavlink": {"connected": true, "tcp_url": "tcp://ados-x.local:5760", "websocket_url": "ws://ados-x.local:8765/"}
        });
        let groups = Dashboard::from_status(&data).reach_links();
        let titles: Vec<&str> = groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            vec![
                "Console",
                "Ground control",
                "Video",
                "Mission Control",
                "Remote"
            ]
        );

        // Console shows both the mDNS and the LAN-IP full URLs, loopback dropped.
        let console: Vec<&str> = groups[0].rows.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            console,
            vec![
                "http://ados-x.local:8080/setup",
                "http://192.168.1.5:8080/setup"
            ]
        );

        // Ground control synthesises tcp + ws on both LAN hosts.
        let gc: Vec<&str> = groups[1].rows.iter().map(|r| r.url.as_str()).collect();
        assert!(gc.contains(&"tcp://ados-x.local:5760"));
        assert!(gc.contains(&"ws://ados-x.local:8765/"));
        assert!(gc.contains(&"tcp://192.168.1.5:5760"));
        assert!(gc.contains(&"ws://192.168.1.5:8765/"));

        // Video is projected onto both LAN hosts too.
        let vid: Vec<&str> = groups[2].rows.iter().map(|r| r.url.as_str()).collect();
        assert!(vid.contains(&"http://ados-x.local:8889/main/"));
        assert!(vid.contains(&"http://192.168.1.5:8889/main/"));
    }

    #[test]
    fn reach_links_drops_empty_groups() {
        let data = json!({
            "access_urls": [{"kind": "setup", "url": "http://ados-x.local:8080/setup", "primary": true}]
        });
        let groups = Dashboard::from_status(&data).reach_links();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "Console");
    }

    #[test]
    fn fc_link_refines_with_api_status() {
        // The gated boolean wins when connected.
        let connected = Dashboard {
            mavlink_connected: true,
            ..Dashboard::default()
        };
        assert_eq!(connected.fc_link(), FcLink::Connected);

        // Transport open but no heartbeat (from /api/status) reads port-open.
        let mut port_open = Dashboard::default();
        port_open.merge_fc_status(&json!({
            "transportOpen": true, "mavlinkAlive": false,
            "fcLinkHint": "msp_detected", "fc_port": "ttyS0", "fc_baud": 921600
        }));
        assert_eq!(port_open.fc_link(), FcLink::PortOpen);
        assert_eq!(port_open.fc_hint().as_deref(), Some("msp detected"));
        assert_eq!(port_open.fc_endpoint().as_deref(), Some("ttyS0 · 921600"));

        // No transport detail at all → down.
        assert_eq!(Dashboard::default().fc_link(), FcLink::Down);
    }

    #[test]
    fn port_of_and_swap_host() {
        assert_eq!(port_of("ws://host:8765/"), Some(8765));
        assert_eq!(port_of("tcp://192.168.1.5:5760"), Some(5760));
        assert_eq!(port_of("http://host/no-port"), None);
        assert_eq!(
            swap_host("http://ados-x.local:8889/main/", "192.168.1.5"),
            "http://192.168.1.5:8889/main/"
        );
        assert_eq!(
            swap_host("ws://ados-x.local:8765/", "10.0.0.9"),
            "ws://10.0.0.9:8765/"
        );
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
