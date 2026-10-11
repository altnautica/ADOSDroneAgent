//! The `radio.tunnel` config slice this service reads, plus the profile gate.
//!
//! ## Two-tier default-off gate (ship inert, opt in after a safety review)
//!
//! - `enabled` (default **false**) is the master opt-in: absent/false ⇒ the
//!   whole config-tunnel path is inert — the drone-side terminator acts on no
//!   config tunnel, and the ground-side injector refuses every request. The
//!   systemd unit additionally gates on the `/etc/ados/tunnel-enabled` marker
//!   that mirrors this field, so an un-opted node never even runs the unit.
//! - `command_enabled` (default **false**) is the second gate on WRITES: with
//!   the channel enabled but this off, a config READ (`GET /api/config`) is
//!   served but a config WRITE (`PUT /api/config`) is refused with an honest
//!   error, mirroring `radio.crsf.mavlink_command_enabled`. It is NOT implied
//!   by opting the channel in; an operator sets it explicitly after a safety
//!   review.
//!
//! This channel carries **config request/response ONLY** — never armed-flight
//! command authority (a separate, gated safety concern). It rides the radio's
//! auxiliary application lane (the drone's `-p2` downlink and the ground
//! station's `-p3` uplink) on its own multiplex channel; the aux transmit and
//! receive pair is brought up ON DEMAND by the radio service, so an opted-out
//! node radiates nothing and spawns nothing. The WFB radio key is shared by a
//! whole fleet, so it is not the gate: every request must carry a relay ticket
//! minted from the per-pair secret for the addressed drone, and a write may
//! touch only `radio.*` and `video.*` keys (never `radio.tunnel.*`), so
//! credentials, cloud and MAVLink routing stay out of the channel's reach.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default loopback ingress port this service binds for inbound TUNNEL frames.
///
/// Defined as the shared constant the rigs' aux plane consumers forward to, so
/// the two sides of the loopback hand-off cannot drift apart on a default.
pub const DEFAULT_RX_PORT: u16 = ados_protocol::config_tunnel_ingest::DEFAULT_INGRESS_PORT;

/// The `radio.tunnel` block of `/etc/ados/config.yaml`.
///
/// `rx_port` is the LOCAL loopback ingress this service binds. Frames do not
/// arrive there off the air directly: the rig's aux plane consumer already holds
/// the plane's own port (only one process may) and forwards this channel's frames
/// on, resolving the same port through
/// [`ados_protocol::config_tunnel_ingest::ingress_port`]. Egress needs no port
/// here — the drone negotiates its transmit ingress with the radio service and
/// the ground station writes to the configured aux uplink ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelChannelConfig {
    /// Master opt-in. Absent/false ⇒ the service idles harmlessly.
    pub enabled: bool,
    /// Whether config WRITES (`PUT /api/config`) are permitted over the radio.
    /// Off by default; a read-only channel until set for a bench-validated
    /// write lane.
    pub command_enabled: bool,
    /// Local loopback ingress port for inbound TUNNEL frames, which the rig's
    /// aux plane consumer forwards this channel's frames to.
    pub rx_port: u16,
}

impl Default for TunnelChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command_enabled: false,
            rx_port: DEFAULT_RX_PORT,
        }
    }
}

/// The `radio.tunnel` slice as it sits in the config. Its `Default` is the
/// schema default, so the slice conformance test holds it to the schema.
#[derive(Debug, Deserialize, Serialize)]
struct TunnelSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    command_enabled: bool,
    #[serde(default)]
    rx_port: Option<u16>,
}

impl Default for TunnelSection {
    fn default() -> Self {
        Self {
            enabled: false,
            command_enabled: false,
            rx_port: Some(DEFAULT_RX_PORT),
        }
    }
}

impl TunnelChannelConfig {
    /// Load from the agent config file. A missing file is the fresh-node case
    /// (the schema defaults, channel off); a malformed file or slice is
    /// reported loudly through the shared slice loader, then degrades to
    /// defaults rather than crash-looping.
    pub fn load_from(path: &Path) -> Self {
        let (t, cfg_err) = match ados_config::schema::load_slice(path, "/radio/tunnel") {
            Ok(t) => (t, None),
            Err(e) => (TunnelSection::default(), Some(e)),
        };
        ados_config::write_config_status("tunnel", cfg_err.as_deref());
        Self::from_section(t)
    }

    fn from_section(t: TunnelSection) -> Self {
        Self {
            enabled: t.enabled,
            command_enabled: t.command_enabled,
            rx_port: t.rx_port.filter(|p| *p != 0).unwrap_or(DEFAULT_RX_PORT),
        }
    }
}

/// Resolve the agent profile as `drone` / `ground-station` / other, reading
/// `agent.profile` from the config file and falling back to the profile marker
/// file. Mirrors the sibling bearer services' profile gate.
pub fn resolve_profile(config_path: &Path, profile_conf: &Path) -> String {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let cfg_profile = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Raw>(&t).ok())
        .and_then(|r| r.agent.profile);
    match cfg_profile.as_deref() {
        Some("drone") => return "drone".to_string(),
        Some("ground_station") | Some("ground-station") => return "ground-station".to_string(),
        _ => {}
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return if v == "drone" {
                    "drone".to_string()
                } else if v == "ground_station" || v == "ground-station" {
                    "ground-station".to_string()
                } else {
                    v.to_string()
                };
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests;
