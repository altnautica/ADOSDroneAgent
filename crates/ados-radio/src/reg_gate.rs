//! Regulatory-gate decision logic and the shared regulatory posture.
//!
//! A pure mapping from a regulatory-domain verification `Result` plus the
//! strict-mode flag to a [`RegGateDecision`], so the gate's branching (proceed /
//! block / proceed-anyway under the lab escape hatch) is unit-testable without
//! standing up `iw`. Also carries the `reg_blocked` state vocabulary, the
//! retry backoff the bring-up loop parks on while blocked, and the [`Posture`]
//! both ends of the link resolve from the same config so they never disagree.

use std::path::Path;

use serde::Deserialize;

use crate::adapter;
use crate::config::{RegulatoryConfig, WfbConfig};

/// The wfb-stats `state` string surfaced while the regulatory gate is blocking.
/// The radio is up but refuses to bring up monitor mode / set a channel until the
/// wanted domain verifies, so it parks here with bounded retry rather than
/// radiating on a band the active domain forbids. Distinct from `no_adapter` /
/// `unpaired` so the panel shows the regulatory conflict in one glance.
pub const STATE_REG_BLOCKED: &str = "reg_blocked";

/// Fixed retry (seconds) for every bring-up park: regulatory-blocked, no
/// injection adapter, channel or PHY that did not land, spawn failure. Fixed
/// and uncapped so a recoverable fault self-heals within seconds; the set call
/// itself carries its own bounded verify retry, so this never spins `iw reg set`
/// in a tight loop.
pub const BRINGUP_RETRY_SECS: u64 = 5;

/// The domain the global reconciler keeps when no `video.wfb.reg_domain` is
/// configured.
pub const DEFAULT_REG_DOMAIN: &str = "US";

/// A pure decision over a regulatory-gate `Result` plus the strict-mode flag.
/// Extracted so the gate's branching (proceed / block / proceed-anyway under the
/// escape hatch) is unit-testable without standing up `iw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegGateDecision {
    /// The gate passed; continue the bring-up.
    Proceed,
    /// The gate failed and strict mode is on; park in `reg_blocked` and retry.
    /// Carries the bland reason code for the log + sidecar.
    Block { reason: &'static str },
    /// The gate failed but strict mode is off (the lab escape hatch); proceed on
    /// a best-effort basis. Carries the reason for the warning log.
    ProceedBestEffort { reason: &'static str },
}

/// Map a gate `Result` + the strict flag to a [`RegGateDecision`]. Pure.
pub fn decide_reg_gate(result: &Result<(), adapter::RegError>, strict: bool) -> RegGateDecision {
    match result {
        Ok(()) => RegGateDecision::Proceed,
        Err(e) => {
            if strict {
                RegGateDecision::Block {
                    reason: e.reason_code(),
                }
            } else {
                RegGateDecision::ProceedBestEffort {
                    reason: e.reason_code(),
                }
            }
        }
    }
}

/// The strict flag in force: the raw `reg_gate_strict` knob applies only under
/// a pinned region; the unrestricted posture never blocks.
pub fn effective_strict(unrestricted: bool, reg_gate_strict: bool) -> bool {
    !unrestricted && reg_gate_strict
}

/// The regulatory posture a radio end brings up under. The drone and the
/// ground station both resolve it from the same agent config, so the two ends
/// set the same domain and apply the same gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posture {
    /// The domain the bring-up sets on the injection PHY. Empty under the
    /// unrestricted posture (`set_reg_domain` no-ops on "").
    pub domain_to_set: String,
    /// The domain the global reconciler keeps and the live read verifies
    /// against: the pinned region, else the configured fallback.
    pub wanted_domain: String,
    /// Whether a gate failure parks the radio in `reg_blocked`.
    pub strict: bool,
}

impl Posture {
    /// Resolve the posture from the `network.regulatory` block plus the
    /// `video.wfb` `reg_domain` / `reg_gate_strict` knobs. Pure.
    pub fn resolve(
        reg: &RegulatoryConfig,
        reg_domain: Option<&str>,
        reg_gate_strict: bool,
    ) -> Posture {
        let fallback = reg_domain
            .filter(|d| !d.is_empty())
            .unwrap_or(DEFAULT_REG_DOMAIN);
        let unrestricted = reg.mode.is_unrestricted();
        let wanted = reg.wanted_domain(fallback).to_string();
        Posture {
            domain_to_set: if unrestricted {
                String::new()
            } else {
                wanted.clone()
            },
            wanted_domain: wanted,
            strict: effective_strict(unrestricted, reg_gate_strict),
        }
    }

    /// Load the posture from the agent config file. Reads quietly (no config
    /// status publish): the owning service already reported its config load.
    pub fn load(config_path: &Path) -> Posture {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            wfb: WfbConfig,
        }
        let reg = RegulatoryConfig::load_from(config_path);
        let wfb = std::fs::read_to_string(config_path)
            .map(|text| {
                ados_config::yaml_or_default::<RawConfig>(&text, "radio")
                    .video
                    .wfb
            })
            .unwrap_or_default();
        Posture::resolve(&reg, wfb.reg_domain.as_deref(), wfb.reg_gate_strict)
    }

    /// True under the unrestricted posture (no domain forced on the PHY).
    pub fn is_unrestricted(&self) -> bool {
        self.domain_to_set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RegulatoryMode;

    #[test]
    fn reg_gate_ok_proceeds() {
        let ok: Result<(), adapter::RegError> = Ok(());
        assert_eq!(decide_reg_gate(&ok, true), RegGateDecision::Proceed);
        assert_eq!(decide_reg_gate(&ok, false), RegGateDecision::Proceed);
    }

    #[test]
    fn reg_gate_strict_failure_blocks_with_reason() {
        let err: Result<(), adapter::RegError> =
            Err(adapter::RegError::ChannelNotEnabled { channel: 165 });
        assert_eq!(
            decide_reg_gate(&err, true),
            RegGateDecision::Block {
                reason: "channel_not_enabled"
            }
        );
    }

    #[test]
    fn reg_gate_eeprom_override_blocks_under_strict() {
        // The live override case: phy bakes a different country than wanted.
        let err: Result<(), adapter::RegError> = Err(adapter::RegError::EepromOverride {
            want: "US".into(),
            got: "BO".into(),
        });
        assert_eq!(
            decide_reg_gate(&err, true),
            RegGateDecision::Block {
                reason: "phy_override"
            }
        );
    }

    #[test]
    fn reg_gate_failure_proceeds_best_effort_when_not_strict() {
        // The lab escape hatch (reg_gate_strict: false) proceeds anyway.
        let err: Result<(), adapter::RegError> = Err(adapter::RegError::VerifyTimeout {
            want: "US".into(),
            got: Some("BO".into()),
        });
        assert_eq!(
            decide_reg_gate(&err, false),
            RegGateDecision::ProceedBestEffort {
                reason: "verify_timeout"
            }
        );
    }

    #[test]
    fn reg_blocked_state_string_is_bland_and_stable() {
        // The sidecar surfaces this verbatim; keep it stable and tag-free.
        assert_eq!(STATE_REG_BLOCKED, "reg_blocked");
    }

    #[test]
    fn unrestricted_posture_never_blocks_regardless_of_raw_flag() {
        // Under the unrestricted operating posture the gate never blocks, even
        // with the raw `reg_gate_strict` flag at its default `true`.
        let strict = effective_strict(true, true);
        assert!(!strict);
        let channel_err: Result<(), adapter::RegError> =
            Err(adapter::RegError::ChannelNotEnabled { channel: 149 });
        assert_eq!(
            decide_reg_gate(&channel_err, strict),
            RegGateDecision::ProceedBestEffort {
                reason: "channel_not_enabled"
            }
        );
        let verify_err: Result<(), adapter::RegError> = Err(adapter::RegError::VerifyTimeout {
            want: "US".into(),
            got: Some("BO".into()),
        });
        assert_eq!(
            decide_reg_gate(&verify_err, strict),
            RegGateDecision::ProceedBestEffort {
                reason: "verify_timeout"
            }
        );
    }

    #[test]
    fn region_posture_keeps_the_strict_gate() {
        let strict = effective_strict(false, true);
        assert!(strict);
        let channel_err: Result<(), adapter::RegError> =
            Err(adapter::RegError::ChannelNotEnabled { channel: 149 });
        assert_eq!(
            decide_reg_gate(&channel_err, strict),
            RegGateDecision::Block {
                reason: "channel_not_enabled"
            }
        );
    }

    #[test]
    fn posture_table() {
        // Unrestricted: nothing forced, never strict, the fallback is kept.
        let unrestricted = Posture::resolve(&RegulatoryConfig::default(), Some("GB"), true);
        assert_eq!(unrestricted.domain_to_set, "");
        assert_eq!(unrestricted.wanted_domain, "GB");
        assert!(!unrestricted.strict);
        assert!(unrestricted.is_unrestricted());
        // No configured fallback → the default domain.
        assert_eq!(
            Posture::resolve(&RegulatoryConfig::default(), None, true).wanted_domain,
            DEFAULT_REG_DOMAIN
        );
        // Region IN: IN is set and wanted, the raw strict flag applies.
        let region = RegulatoryConfig {
            mode: RegulatoryMode::Region,
            region: Some("IN".to_string()),
            ..RegulatoryConfig::default()
        };
        for cfg_strict in [true, false] {
            let p = Posture::resolve(&region, Some("US"), cfg_strict);
            assert_eq!(p.domain_to_set, "IN");
            assert_eq!(p.wanted_domain, "IN");
            assert_eq!(p.strict, cfg_strict);
            assert!(!p.is_unrestricted());
        }
    }

    #[test]
    fn posture_load_is_identical_for_the_drone_and_the_ground_station() {
        // Both ends call `Posture::load` on the same agent config; the profile
        // does not enter the resolution.
        let dir = tempfile::tempdir().unwrap();
        let drone = dir.path().join("drone.yaml");
        let ground = dir.path().join("ground.yaml");
        let body = "network:\n  regulatory:\n    mode: region\n    region: in\n\
                    video:\n  wfb:\n    reg_domain: US\n    reg_gate_strict: false\n";
        std::fs::write(&drone, format!("agent:\n  profile: drone\n{body}")).unwrap();
        std::fs::write(
            &ground,
            format!("agent:\n  profile: ground_station\n{body}"),
        )
        .unwrap();
        let d = Posture::load(&drone);
        let g = Posture::load(&ground);
        assert_eq!(d, g);
        assert_eq!(d.domain_to_set, "IN");
        assert!(!d.strict);
        // An absent file is the unrestricted default.
        let none = Posture::load(&dir.path().join("absent.yaml"));
        assert!(none.is_unrestricted());
        assert_eq!(none.wanted_domain, DEFAULT_REG_DOMAIN);
    }
}
