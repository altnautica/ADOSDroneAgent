//! The bearer-agnostic transport contract.

use serde::{Deserialize, Serialize};

use ados_protocol::atlas::AtlasEvent;

use crate::TransportError;

/// Which carrier a bearer is. The variant order is local-first:
/// loopback and direct LAN before the WFB relay before the cloud lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BearerKind {
    /// In-process channel (tests, same-host).
    Loopback,
    /// Direct LAN/WiFi HTTP — the first-class production path.
    DirectLan,
    /// Post-flight bulk upload over the LAN.
    PostFlightBulk,
    /// Relayed WFB<->LAN by the ground agent (field / outdoor).
    WfbRelay,
    /// MQTT/Convex cloud relay (off-LAN, opt-in).
    Cloud,
}

impl BearerKind {
    /// Selection priority, lower preferred. The ladder tries bearers in this
    /// order, so a usable local bearer always wins over the cloud lane.
    pub fn priority(self) -> u8 {
        match self {
            Self::Loopback => 0,
            Self::DirectLan => 1,
            Self::PostFlightBulk => 2,
            Self::WfbRelay => 3,
            Self::Cloud => 4,
        }
    }
}

/// One carrier for the Atlas stream lane. Bearers are interchangeable: the
/// framed [`AtlasEvent`] is identical on every one, so the [`crate::BearerLadder`]
/// can fail over from one to the next without the world-model contract changing.
#[async_trait::async_trait]
pub trait AtlasBearer: Send + Sync {
    /// The carrier this bearer is.
    fn kind(&self) -> BearerKind;

    /// Whether the bearer is currently usable. The ladder skips an unavailable
    /// bearer rather than failing a send against it.
    async fn is_available(&self) -> bool;

    /// Send one framed event over this bearer.
    async fn send(&self, event: &AtlasEvent) -> Result<(), TransportError>;
}
