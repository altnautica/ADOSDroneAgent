//! Regulatory-gate decision logic for the WFB bring-up.
//!
//! A pure mapping from a regulatory-domain verification `Result` plus the
//! strict-mode flag to a [`RegGateDecision`], so the gate's branching (proceed /
//! block / proceed-anyway under the lab escape hatch) is unit-testable without
//! standing up `iw`. Also carries the `reg_blocked` state vocabulary + the
//! retry backoff the bring-up loop parks on while blocked.

use ados_radio::adapter;

/// The wfb-stats `state` string surfaced while the regulatory gate is blocking.
/// The radio is up but refuses to bring up monitor mode / set a channel until the
/// wanted domain verifies, so it parks here with bounded retry rather than
/// radiating on a band the active domain forbids. Distinct from `no_adapter` /
/// `unpaired` so the panel shows the regulatory conflict in one glance.
pub(crate) const STATE_REG_BLOCKED: &str = "reg_blocked";

/// Backoff (seconds) between regulatory-gate retries while blocked. Bounded and
/// short so a transient domain glitch self-heals quickly, but slow enough not to
/// spin `iw reg set` in a tight loop.
pub(crate) const REG_BLOCKED_RETRY_SECS: u64 = 10;

/// A pure decision over a regulatory-gate `Result` plus the strict-mode flag.
/// Extracted so the gate's branching (proceed / block / proceed-anyway under the
/// escape hatch) is unit-testable without standing up `iw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegGateDecision {
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
pub(crate) fn decide_reg_gate(
    result: &Result<(), adapter::RegError>,
    strict: bool,
) -> RegGateDecision {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The effective strict flag passed to `decide_reg_gate` from the bring-up,
    /// mirroring the call sites' `!unrestricted && cfg.reg_gate_strict`. Under the
    /// unrestricted posture this is always false regardless of the raw flag.
    fn effective_strict(unrestricted: bool, reg_gate_strict: bool) -> bool {
        !unrestricted && reg_gate_strict
    }

    #[test]
    fn unrestricted_posture_never_blocks_regardless_of_raw_flag() {
        // Under the unrestricted operating posture the gate never blocks, even
        // with the raw `reg_gate_strict` flag at its default `true`. Exercise all
        // three failure variants the strict gate would otherwise park on.
        let unrestricted = true;
        let raw_strict = true; // the retained default
        let strict = effective_strict(unrestricted, raw_strict);
        assert!(!strict);

        let channel_err: Result<(), adapter::RegError> =
            Err(adapter::RegError::ChannelNotEnabled { channel: 149 });
        assert_eq!(
            decide_reg_gate(&channel_err, strict),
            RegGateDecision::ProceedBestEffort {
                reason: "channel_not_enabled"
            }
        );

        let eeprom_err: Result<(), adapter::RegError> = Err(adapter::RegError::EepromOverride {
            want: "US".into(),
            got: "BO".into(),
        });
        assert_eq!(
            decide_reg_gate(&eeprom_err, strict),
            RegGateDecision::ProceedBestEffort {
                reason: "phy_override"
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
        // Pinning a region restores the strict gate: the same failure that
        // proceeds best-effort under unrestricted blocks under a pinned region.
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
}
