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
    /// A one-line description of what this group's links are for (empty = none).
    pub desc: &'static str,
    pub rows: Vec<LinkRow>,
}

/// A setup-wizard step with its raw state (e.g. `complete`, `needs_action`).
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub label: String,
    pub state: String,
    /// A human-readable description of the step, or why it needs action. Empty
    /// when the agent advertised none.
    pub detail: String,
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
    /// An MSP flight controller (Betaflight/iNav) identified by its USB
    /// descriptor: attached + recognised, but MAVLink telemetry does not apply.
    Msp,
    /// Transport open but no HEARTBEAT decoded (wrong baud, or a booting FC).
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
    /// The Links panel groups, derived once from the advertised URLs when the
    /// model is built rather than on every rendered frame.
    pub links: Vec<LinkGroup>,

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
    /// FC firmware family from the USB descriptor (`betaflight`/`inav`), merged
    /// from `/api/status`; drives the MSP badge.
    pub fc_variant: Option<String>,
    /// The canonical FC firmware family (`ardupilot`/`px4`/`betaflight`/`inav`),
    /// merged from `/api/status`; names ArduPilot vs PX4 for a MAVLink FC, which
    /// `fc_variant` cannot. `None` when the family is unknown / unverified.
    pub fc_firmware: Option<String>,

    pub video_state: String,
    pub video_viewer: Option<String>,

    pub cloud_relay: String,
    pub cloud_mode: String,
    pub cloud_paired: bool,
    pub cloud_configured: bool,
    pub remote_status: String,
    pub hotspot: String,
    /// The access point's passphrase, read off the box.
    ///
    /// Shown because it is now generated per unit rather than being one
    /// published default on every box: this and the installer's closing card
    /// are how an operator learns it at all. Empty when the hotspot is off or
    /// the file is unreadable — never a placeholder, since a wrong passphrase
    /// is indistinguishable from a broken radio to the person typing it.
    pub hotspot_passphrase: String,

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

mod urls;
use urls::*;
mod from_status;
mod reach;

/// A reachable host used to project per-host service URLs (mDNS name or LAN IP).
struct HostRef {
    host: String,
    primary: bool,
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
        self.fc_variant = opt_str(status, "fcVariant");
        // The canonical family; drop the honest `unknown` sentinel so the label
        // helper falls back to a generic "FC" rather than naming "Unknown".
        self.fc_firmware = opt_str(status, "fcFirmware").filter(|f| f != "unknown");
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
        } else if self.fc_variant.is_some() {
            // An MSP FC identified by its USB descriptor: attached + recognised
            // (it never sends a MAVLink heartbeat), so not a "not connected".
            FcLink::Msp
        } else if self.transport_open == Some(true) && self.mavlink_alive == Some(false) {
            FcLink::PortOpen
        } else {
            FcLink::Down
        }
    }

    /// The display name of the identified FC firmware family, e.g. `ArduPilot`
    /// / `PX4` / `Betaflight` / `iNav`. `None` when the family is unknown, so a
    /// caller falls back to a generic label rather than naming a guess.
    pub fn fc_family_label(&self) -> Option<String> {
        self.fc_firmware.as_deref().and_then(|f| match f {
            "ardupilot" => Some("ArduPilot".to_string()),
            "px4" => Some("PX4".to_string()),
            "betaflight" => Some("Betaflight".to_string()),
            "inav" => Some("iNav".to_string()),
            _ => None,
        })
    }

    /// The display label for an identified MSP FC, e.g. `Betaflight` / `iNav`.
    pub fn fc_variant_label(&self) -> Option<String> {
        self.fc_variant.as_deref().map(|v| match v {
            "betaflight" => "Betaflight".to_string(),
            "inav" => "iNav".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => other.to_string(),
                }
            }
        })
    }

    /// A full, actionable link-hint sentence for the FC sub-line, expanded from
    /// the router's short token (`no_heartbeat` / `msp_detected`) so the operator
    /// knows what to check. An unknown token degrades to its spaced form.
    pub fn fc_hint(&self) -> Option<String> {
        self.fc_link_hint.as_ref().map(|h| match h.as_str() {
            "no_heartbeat" => {
                "Serial device found but no MAVLink heartbeat — check the FC baud rate, power, and wiring."
                    .to_string()
            }
            "msp_detected" => {
                // MSP is the native, terminal protocol for Betaflight/iNav — a
                // valid link, not a misconfiguration to fix.
                "Flight controller speaking MSP (Betaflight/iNav) — a valid native link.".to_string()
            }
            other => other.replace('_', " "),
        })
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
        // An identified MSP FC is a live link even though it never sends the
        // MAVLink heartbeat `mavlink_connected` gates on.
        let fc_ok = matches!(self.fc_link(), FcLink::Connected | FcLink::Msp) || is_ground_station;
        let configured = self.paired || (self.has_steps && self.steps_all_complete);

        if services_failed {
            Health::Degraded
        } else if configured && !fc_ok {
            // A set-up drone that has lost its flight controller.
            Health::Degraded
        } else if services_all_running && configured && fc_ok {
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
mod tests;
