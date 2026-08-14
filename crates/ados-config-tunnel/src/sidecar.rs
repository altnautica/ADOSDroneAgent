//! The Contract E sidecar: `/run/ados/tunnel-config.json`.
//!
//! Written atomically ~1 Hz while the service runs (and once on a terminal
//! state), so a staleness-gated reader (`ados-control`, the GCS via L4) can
//! tell a live idle channel from a dead service's orphaned file. Every field
//! is honest: `state` reflects the real role, the counters are received-side
//! proof (a `rx_frames` that never advances means nothing arrives over the
//! bearer), and `last_rx_ms` is `null` until a real frame lands — never a
//! fabricated green.

use serde_json::{json, Value};

use crate::paths::TUNNEL_CONFIG_SIDECAR_VERSION;
use crate::stats::CountersSnapshot;

/// The channel's role/state for the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// Not opted in (or the wrong profile / no marker): the service idles.
    Disabled,
    /// The drone-side terminator is running (serves config requests off the
    /// bearer).
    Terminator,
    /// The ground-side injector is running (emits requests + awaits replies).
    Injector,
}

impl ChannelState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Terminator => "terminator",
            Self::Injector => "injector",
        }
    }
}

/// The inputs one sidecar write reports.
#[derive(Debug, Clone, Copy)]
pub struct SidecarInputs {
    pub state: ChannelState,
    /// The master opt-in.
    pub enabled: bool,
    /// The WRITE gate: `false` ⇒ reads served, writes refused.
    pub command_enabled: bool,
    /// The loopback ingress this service binds, when the channel is active.
    pub rx_port: Option<u16>,
    /// The aux transmit ingress this rig writes onto, or `None` where the radio
    /// service negotiates it per open (the drone role) — never a fabricated
    /// number for a port this process does not actually use.
    pub tx_port: Option<u16>,
    pub counters: CountersSnapshot,
}

/// Serialize the sidecar body.
#[must_use]
pub fn build_sidecar(inputs: &SidecarInputs) -> Value {
    let c = &inputs.counters;
    json!({
        "v": TUNNEL_CONFIG_SIDECAR_VERSION,
        "state": inputs.state.as_str(),
        "enabled": inputs.enabled,
        // The WRITE gate, surfaced so an operator sees whether config writes
        // over the radio are open (default closed until a safety review).
        "command_enabled": inputs.command_enabled,
        // Honest scope: this channel carries config request/response ONLY —
        // never armed-flight command authority — and rides the radio's auxiliary
        // application lane on its own multiplex channel (the WFB pairing key is
        // its only gate).
        "carries": "config",
        "bearer": "aux",
        "rx_port": inputs.rx_port,
        "tx_port": inputs.tx_port,
        "rx_frames": c.rx_frames,
        "tx_frames": c.tx_frames,
        "requests": c.requests,
        "responses": c.responses,
        "rejected": c.rejected,
        "timeouts": c.timeouts,
        // null until a real frame lands — the received-side delivery proof.
        "last_rx_ms": (c.last_rx_ms != 0).then_some(c.last_rx_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_sidecar_is_honest() {
        let body = build_sidecar(&SidecarInputs {
            state: ChannelState::Disabled,
            enabled: false,
            command_enabled: false,
            rx_port: None,
            tx_port: None,
            counters: CountersSnapshot::default(),
        });
        assert_eq!(body["state"], "disabled");
        assert_eq!(body["enabled"], false);
        assert_eq!(body["carries"], "config");
        // The bearer the service actually rides. It read `-p1` while the code
        // has never touched that plane, which is exactly the class of surface
        // an operator makes a decision on.
        assert_eq!(body["bearer"], "aux");
        assert_eq!(body["last_rx_ms"], Value::Null);
        assert_eq!(body["v"], TUNNEL_CONFIG_SIDECAR_VERSION);
    }

    #[test]
    fn an_active_terminator_reports_its_ingress_and_counters() {
        let body = build_sidecar(&SidecarInputs {
            state: ChannelState::Terminator,
            enabled: true,
            command_enabled: false,
            rx_port: Some(5820),
            // The drone role: the radio service negotiates the transmit ingress
            // per open, so there is no static egress port to claim.
            tx_port: None,
            counters: CountersSnapshot {
                rx_frames: 3,
                requests: 1,
                last_rx_ms: 42,
                ..CountersSnapshot::default()
            },
        });
        assert_eq!(body["state"], "terminator");
        assert_eq!(body["command_enabled"], false);
        assert_eq!(body["rx_port"], 5820);
        assert_eq!(body["tx_port"], Value::Null);
        assert_eq!(body["rx_frames"], 3);
        assert_eq!(body["requests"], 1);
        assert_eq!(body["last_rx_ms"], 42);
    }

    #[test]
    fn an_active_injector_reports_the_aux_transmit_ingress() {
        let body = build_sidecar(&SidecarInputs {
            state: ChannelState::Injector,
            enabled: true,
            command_enabled: true,
            rx_port: Some(5820),
            // The ground station writes straight onto the already-running aux
            // uplink transmit ingress.
            tx_port: Some(5602),
            counters: CountersSnapshot::default(),
        });
        assert_eq!(body["state"], "injector");
        assert_eq!(body["tx_port"], 5602);
    }
}
