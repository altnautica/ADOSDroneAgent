//! Structured regulatory-gate verdict events for the logging daemon.
//!
//! The radio gates its bring-up on the regulatory domain twice: once to set and
//! verify the global domain before the adapter enters monitor mode, and once to
//! assert the rendezvous channel is in the domain's enabled, non-DFS set after
//! the wiphy exists. Both verdicts were previously only `tracing` log lines plus
//! a sidecar snapshot. This module turns each verdict into a discrete, queryable
//! event so an RCA can ask "what did the reg gate decide, on which band/channel,
//! under which country" without scraping logs.
//!
//! The detail map is built purely (testable without the daemon) and shipped via
//! the shared event emitter. The existing log lines and the wfb-stats sidecar
//! are unchanged; this is purely additive durable capture.

use ados_protocol::logd::{Fields, Level, Value as MpVal};

/// The event kind for a regulatory-gate verdict.
pub const REG_GATE_KIND: &str = "radio.reg_gate";

/// Which gate stage produced the verdict: the global domain set/verify, or the
/// per-channel enabled/DFS assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegGateStage {
    /// Set + verify the global regulatory domain before monitor mode.
    Domain,
    /// Assert the rendezvous channel is enabled and non-DFS for the domain.
    Channel,
}

impl RegGateStage {
    /// A bland, stable stage tag carried in the event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            RegGateStage::Domain => "domain",
            RegGateStage::Channel => "channel",
        }
    }
}

/// The verdict outcome carried in the event's `result` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegGateResult {
    /// The gate passed; the bring-up continues.
    Ok,
    /// The gate failed under the strict gate; the radio parks and retries.
    Blocked,
    /// The gate failed but the escape hatch let the bring-up proceed anyway.
    Failed,
}

impl RegGateResult {
    /// The `result` string in the event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            RegGateResult::Ok => "ok",
            RegGateResult::Blocked => "blocked",
            RegGateResult::Failed => "failed",
        }
    }

    /// The event severity: a clean pass is informational; any failure (blocked or
    /// proceeded-best-effort) is a warning so it surfaces in a severity filter.
    pub fn severity(self) -> Level {
        match self {
            RegGateResult::Ok => Level::Info,
            RegGateResult::Blocked | RegGateResult::Failed => Level::Warn,
        }
    }
}

/// Build the `radio.reg_gate` detail map. All fields are bland and reader-facing:
///
/// - `stage` — `domain` | `channel` (which gate produced the verdict);
/// - `band` — the configured band (e.g. `u-nii-3`);
/// - `channel` — the rendezvous channel;
/// - `requested_country` — the domain the gate asked for;
/// - `applied_country` — the live domain `iw reg get` reports (when readable);
/// - `eeprom_override` — true when a self-managed phy's baked country overrode
///   the global set (the unrecoverable conflict);
/// - `result` — `ok` | `blocked` | `failed`;
/// - `reason` — the gate's bland reason code, present only on a failure.
#[allow(clippy::too_many_arguments)]
pub fn reg_gate_detail(
    stage: RegGateStage,
    band: &str,
    channel: u8,
    requested_country: &str,
    applied_country: Option<&str>,
    eeprom_override: bool,
    result: RegGateResult,
    reason: Option<&str>,
) -> Fields {
    let mut d = Fields::new();
    d.insert("stage".to_string(), MpVal::from(stage.as_str()));
    d.insert("band".to_string(), MpVal::from(band));
    d.insert("channel".to_string(), MpVal::from(channel as u64));
    d.insert(
        "requested_country".to_string(),
        MpVal::from(requested_country),
    );
    if let Some(applied) = applied_country {
        d.insert("applied_country".to_string(), MpVal::from(applied));
    }
    d.insert("eeprom_override".to_string(), MpVal::from(eeprom_override));
    d.insert("result".to_string(), MpVal::from(result.as_str()));
    if let Some(reason) = reason {
        d.insert("reason".to_string(), MpVal::from(reason));
    }
    d
}

/// True when a gate error is the unrecoverable self-managed-phy override (a
/// baked country the global `iw reg set` could not displace). Used to set the
/// `eeprom_override` flag on the verdict without leaking the internal variant.
pub fn is_eeprom_override(err: &crate::adapter::RegError) -> bool {
    matches!(err, crate::adapter::RegError::EepromOverride { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::RegError;

    #[test]
    fn ok_verdict_carries_no_reason_and_is_info() {
        let d = reg_gate_detail(
            RegGateStage::Domain,
            "u-nii-3",
            149,
            "US",
            Some("US"),
            false,
            RegGateResult::Ok,
            None,
        );
        assert_eq!(d.get("stage").and_then(|v| v.as_str()), Some("domain"));
        assert_eq!(d.get("band").and_then(|v| v.as_str()), Some("u-nii-3"));
        assert_eq!(d.get("channel").and_then(|v| v.as_u64()), Some(149));
        assert_eq!(
            d.get("requested_country").and_then(|v| v.as_str()),
            Some("US")
        );
        assert_eq!(
            d.get("applied_country").and_then(|v| v.as_str()),
            Some("US")
        );
        assert_eq!(
            d.get("eeprom_override").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(d.get("result").and_then(|v| v.as_str()), Some("ok"));
        assert!(!d.contains_key("reason"));
        assert_eq!(RegGateResult::Ok.severity(), Level::Info);
    }

    #[test]
    fn blocked_channel_verdict_carries_reason_and_is_warn() {
        let d = reg_gate_detail(
            RegGateStage::Channel,
            "u-nii-3",
            165,
            "US",
            Some("BO"),
            false,
            RegGateResult::Blocked,
            Some("channel_not_enabled"),
        );
        assert_eq!(d.get("stage").and_then(|v| v.as_str()), Some("channel"));
        assert_eq!(d.get("result").and_then(|v| v.as_str()), Some("blocked"));
        assert_eq!(
            d.get("reason").and_then(|v| v.as_str()),
            Some("channel_not_enabled")
        );
        assert_eq!(
            d.get("applied_country").and_then(|v| v.as_str()),
            Some("BO")
        );
        assert_eq!(RegGateResult::Blocked.severity(), Level::Warn);
    }

    #[test]
    fn failed_best_effort_verdict_is_warn() {
        let d = reg_gate_detail(
            RegGateStage::Domain,
            "u-nii-3",
            149,
            "US",
            None,
            false,
            RegGateResult::Failed,
            Some("verify_timeout"),
        );
        assert_eq!(d.get("result").and_then(|v| v.as_str()), Some("failed"));
        assert_eq!(
            d.get("reason").and_then(|v| v.as_str()),
            Some("verify_timeout")
        );
        // No applied_country read available on the best-effort path.
        assert!(!d.contains_key("applied_country"));
        assert_eq!(RegGateResult::Failed.severity(), Level::Warn);
    }

    #[test]
    fn eeprom_override_only_for_the_phy_override_variant() {
        assert!(is_eeprom_override(&RegError::EepromOverride {
            want: "US".to_string(),
            got: "BO".to_string(),
        }));
        assert!(!is_eeprom_override(&RegError::ChannelNotEnabled {
            channel: 165
        }));
        assert!(!is_eeprom_override(&RegError::CommandFailed));
    }
}
