//! The operator-facing `network.regulatory` posture.

use serde::Deserialize;

/// Operator-facing regulatory posture, read from the `network.regulatory:` block
/// of `/etc/ados/config.yaml`. The DEFAULT is unrestricted: a fresh box with no
/// block radiates on the operator's configured channel at hardware-bounded power
/// without requiring a verified operating region. The operator opts INTO a region
/// to re-enable the strict regulatory gate (channel/domain enforcement for that
/// jurisdiction). This is a separate, higher-level switch over the underlying
/// `video.wfb.{reg_gate_strict,reg_domain,dfs_allowed}` knobs — the mode gates
/// them, it does not change their raw defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegulatoryMode {
    /// Radio brings up + TX-enables on the home channel with no verified region.
    /// The operator is responsible for legal RF operation in their location.
    #[default]
    Unrestricted,
    /// An operating region is pinned; the strict regulatory gate is enforced for
    /// that jurisdiction (today's fail-closed behaviour for the pinned region).
    Region,
}

impl RegulatoryMode {
    /// Parse the wire string. Anything other than the explicit `region` token
    /// (case-insensitive) reads as unrestricted, so a fresh / malformed value is
    /// permissive rather than fail-closed (the policy default).
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "region" => RegulatoryMode::Region,
            _ => RegulatoryMode::Unrestricted,
        }
    }

    /// The wire string persisted in config + surfaced on the heartbeat.
    pub fn as_wire(self) -> &'static str {
        match self {
            RegulatoryMode::Unrestricted => "unrestricted",
            RegulatoryMode::Region => "region",
        }
    }

    /// True when the strict regulatory gate / region enforcement is OFF.
    pub fn is_unrestricted(self) -> bool {
        matches!(self, RegulatoryMode::Unrestricted)
    }
}

/// The operating-region knob the three operator surfaces write. Defaults to
/// unrestricted with no pinned region. `region` is an ISO 3166-1 alpha-2 country
/// code (uppercase) when `mode == Region`, else `None`. `ack_operator` / `ack_at`
/// record who chose the posture and when (audit), and never affect behaviour.
#[derive(Debug, Clone, Default)]
pub struct RegulatoryConfig {
    pub mode: RegulatoryMode,
    pub region: Option<String>,
    pub ack_operator: Option<String>,
    pub ack_at: Option<String>,
}

impl RegulatoryConfig {
    /// Load the `network.regulatory:` block from the agent config file. An absent
    /// file or block reads as the unrestricted default (the fresh-box posture).
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            network: NetworkSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct NetworkSection {
            #[serde(default)]
            regulatory: Option<RawRegulatory>,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return RegulatoryConfig::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "radio");
        raw.network
            .regulatory
            .map(RawRegulatory::resolve)
            .unwrap_or_default()
    }

    /// The wanted regulatory domain this posture resolves to, given the configured
    /// `reg_domain` fallback (the `video.wfb.reg_domain` default, "US"). Region →
    /// the pinned region code; Unrestricted → the configured fallback (so the
    /// onboard-WiFi global reconciler still has a sane domain to keep, never `00`).
    pub fn wanted_domain<'a>(&'a self, unrestricted_fallback: &'a str) -> &'a str {
        match self.mode {
            RegulatoryMode::Region => self.region.as_deref().unwrap_or(unrestricted_fallback),
            RegulatoryMode::Unrestricted => unrestricted_fallback,
        }
    }
}

/// The raw `network.regulatory` shape as it appears on disk. Resolved into a
/// [`RegulatoryConfig`] so the mode string and region casing are normalised in
/// one place. A `region` mode with no region code degrades to unrestricted (there
/// is no jurisdiction to enforce), which is the safe-permissive direction.
#[derive(Debug, Default, Deserialize)]
struct RawRegulatory {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    ack_operator: Option<String>,
    #[serde(default)]
    ack_at: Option<String>,
}

impl RawRegulatory {
    fn resolve(self) -> RegulatoryConfig {
        let region = self
            .region
            .map(|r| r.trim().to_ascii_uppercase())
            .filter(|r| !r.is_empty());
        let mode = match self.mode.as_deref().map(RegulatoryMode::from_wire) {
            Some(RegulatoryMode::Region) if region.is_some() => RegulatoryMode::Region,
            // region mode with no code, or an absent/unrestricted mode → permissive
            _ => RegulatoryMode::Unrestricted,
        };
        RegulatoryConfig {
            mode,
            region: if mode == RegulatoryMode::Region {
                region
            } else {
                None
            },
            ack_operator: self.ack_operator,
            ack_at: self.ack_at,
        }
    }
}
