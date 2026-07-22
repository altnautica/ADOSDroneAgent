//! Regulatory posture for the RC lane: policy read, knob application, and the
//! durable bring-up event behind the operator-responsibility badge.
//!
//! The RC transmitter module owns its own PHY and its own regulatory tables —
//! the lane NEVER reaches for the system radio stack (`iw`, modprobe options,
//! the WFB domain reconciler; those govern the WFB PHY only). The lane's
//! regulatory levers are:
//!
//! * the **host frame cadence** (`radio.crsf.packet_rate_hz`), applied
//!   directly by the transmit task — genuinely in force the moment the lane
//!   runs; and
//! * the module-side knobs (**band**, **conducted TX power**), which live in
//!   the module's own parameter system and change ONLY via CRSF parameter
//!   writes (settings entry `0x2D` / read `0x2E` / write `0x2F`) carried on
//!   the lane's out-of-band frame queue. The lane is a transparent carrier
//!   for that registry — the module's parameter indices are firmware-defined
//!   and enumerated by the configuration surface, so the agent records the
//!   configured targets and carries the writes; it never guesses an index.
//!
//! The `network.regulatory` block is read as POLICY (the single operator
//! knob shared with the other RF surfaces): `unrestricted` (the default)
//! means the module runs at the operator's configured values up to its own
//! hardware ceiling with the operator responsible for legal RF operation;
//! `region` records a pinned jurisdiction. Either way the posture, the
//! configured knob targets, and the operator-acknowledgement state are
//! emitted as one durable `link.crsf_reg_posture` event at every link
//! bring-up, so any status surface can carry the amber
//! "operator responsible for local RF compliance" badge truthfully instead
//! of asserting a compliance state nobody verified.

use std::path::Path;

use ados_protocol::logd::{Fields, Level, Value as MpVal};
use serde::Deserialize;

use crate::config::CrsfLaneConfig;

/// The event kind for the bring-up regulatory-posture record.
pub const REG_POSTURE_KIND: &str = "link.crsf_reg_posture";

/// The operator-facing regulatory posture (`network.regulatory.mode`).
/// Default unrestricted: anything but the explicit `region` token (with a
/// usable region code) reads permissive, matching the fleet-wide posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegMode {
    #[default]
    Unrestricted,
    Region,
}

impl RegMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RegMode::Unrestricted => "unrestricted",
            RegMode::Region => "region",
        }
    }
}

/// The `network.regulatory` block, resolved: region uppercased and present
/// only in region mode (a `region` mode with no code degrades to
/// unrestricted — there is no jurisdiction to enforce). The ack fields are
/// audit data recorded with the posture choice; they never change behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegPolicy {
    pub mode: RegMode,
    pub region: Option<String>,
    pub ack_operator: Option<String>,
    pub ack_at: Option<String>,
}

impl RegPolicy {
    /// Load from the agent config file. An absent file/block reads as the
    /// unrestricted default (the fresh-node posture); a malformed file is the
    /// config loader's to report — this secondary read degrades quietly.
    pub fn load_from(path: &Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct Raw {
            #[serde(default)]
            network: NetworkSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct NetworkSection {
            #[serde(default)]
            regulatory: Option<RawRegulatory>,
        }
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
        let Ok(text) = std::fs::read_to_string(path) else {
            return RegPolicy::default();
        };
        let Ok(raw) = serde_norway::from_str::<Raw>(&text) else {
            return RegPolicy::default();
        };
        let Some(reg) = raw.network.regulatory else {
            return RegPolicy::default();
        };
        let region = reg
            .region
            .map(|r| r.trim().to_ascii_uppercase())
            .filter(|r| !r.is_empty());
        let mode = match reg.mode.as_deref().map(str::trim) {
            Some(m) if m.eq_ignore_ascii_case("region") && region.is_some() => RegMode::Region,
            _ => RegMode::Unrestricted,
        };
        RegPolicy {
            region: if mode == RegMode::Region {
                region
            } else {
                None
            },
            mode,
            ack_operator: reg.ack_operator,
            ack_at: reg.ack_at,
        }
    }

    /// True when an operator acknowledgement is on record for the posture.
    pub fn operator_acked(&self) -> bool {
        self.ack_operator.is_some()
    }
}

/// Build the `link.crsf_reg_posture` detail map (pure, testable). Fields:
///
/// - `posture` / `region` — the policy (`region` present only when pinned);
/// - `operator_acked` / `ack_operator` / `ack_at` — the recorded
///   acknowledgement state behind the responsibility badge;
/// - `packet_rate_hz` + `packet_rate_applied: true` — the host frame cadence,
///   in force on this bring-up (the transmit task ticks at exactly this rate);
/// - `band_target` — the configured module band class; `tx_power_target_dbm`
///   — the configured conducted-power target, present only when the operator
///   set one (absent = the module's own default, nothing to write);
/// - `module_knob_mechanism: "crsf_lua_parameter"` — how the module-side
///   knobs change (the lane's parameter-write carrier), never a system radio
///   operation; `module_knobs_verified: false` — honest: this event records
///   targets and posture, not a module read-back (the measured TX power
///   arrives separately on link-statistics telemetry via the sidecar).
pub fn reg_posture_detail(cfg: &CrsfLaneConfig, policy: &RegPolicy) -> Fields {
    let mut d = Fields::new();
    d.insert("posture".to_string(), MpVal::from(policy.mode.as_str()));
    if let Some(region) = policy.region.as_deref() {
        d.insert("region".to_string(), MpVal::from(region));
    }
    d.insert(
        "operator_acked".to_string(),
        MpVal::from(policy.operator_acked()),
    );
    if let Some(op) = policy.ack_operator.as_deref() {
        d.insert("ack_operator".to_string(), MpVal::from(op));
    }
    if let Some(at) = policy.ack_at.as_deref() {
        d.insert("ack_at".to_string(), MpVal::from(at));
    }
    d.insert(
        "packet_rate_hz".to_string(),
        MpVal::from(u64::from(cfg.packet_rate_hz)),
    );
    d.insert("packet_rate_applied".to_string(), MpVal::from(true));
    d.insert("band_target".to_string(), MpVal::from(cfg.band.as_str()));
    if let Some(dbm) = cfg.tx_power_dbm {
        d.insert("tx_power_target_dbm".to_string(), MpVal::from(dbm));
    }
    d.insert(
        "module_knob_mechanism".to_string(),
        MpVal::from("crsf_lua_parameter"),
    );
    d.insert("module_knobs_verified".to_string(), MpVal::from(false));
    d
}

/// The event severity: an unrestricted posture with NO recorded operator
/// acknowledgement warrants a warning (the badge has no ack behind it); an
/// acknowledged or region-pinned posture is informational.
pub fn reg_posture_severity(policy: &RegPolicy) -> Level {
    match policy.mode {
        RegMode::Unrestricted if !policy.operator_acked() => Level::Warn,
        _ => Level::Info,
    }
}

/// Emit the bring-up posture event. Best-effort (the emitter drops on an
/// absent logging daemon); called once per serial bring-up so the durable
/// store carries one record per link session.
pub fn emit_reg_posture(
    metrics: &ados_protocol::logd::emitter::IngestEmitter,
    cfg: &CrsfLaneConfig,
    policy: &RegPolicy,
) {
    metrics.emit_event(
        REG_POSTURE_KIND,
        reg_posture_severity(policy),
        reg_posture_detail(cfg, policy),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BandMode;
    use std::io::Write;

    fn write_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_or_block_reads_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            RegPolicy::load_from(&dir.path().join("nope.yaml")),
            RegPolicy::default()
        );
        let bare = write_file(&dir, "bare.yaml", "agent:\n  profile: ground_station\n");
        let p = RegPolicy::load_from(&bare);
        assert_eq!(p.mode, RegMode::Unrestricted);
        assert!(p.region.is_none());
        assert!(!p.operator_acked());
    }

    #[test]
    fn region_mode_reads_and_uppercases_with_ack_audit() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "network:\n  regulatory:\n    mode: region\n    region: \" in \"\n    ack_operator: op-1\n    ack_at: \"2026-07-23T00:00:00+05:30\"\n",
        );
        let p = RegPolicy::load_from(&path);
        assert_eq!(p.mode, RegMode::Region);
        assert_eq!(p.region.as_deref(), Some("IN"));
        assert!(p.operator_acked());
        assert_eq!(p.ack_at.as_deref(), Some("2026-07-23T00:00:00+05:30"));
    }

    #[test]
    fn region_mode_without_a_code_degrades_to_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "config.yaml",
            "network:\n  regulatory:\n    mode: region\n    region: \"\"\n",
        );
        let p = RegPolicy::load_from(&path);
        assert_eq!(p.mode, RegMode::Unrestricted);
        assert!(p.region.is_none());
    }

    #[test]
    fn detail_carries_posture_knobs_and_ack_state() {
        let cfg = CrsfLaneConfig {
            enabled: true,
            band: BandMode::Band900,
            packet_rate_hz: 150,
            tx_power_dbm: Some(20),
            ..Default::default()
        };
        let policy = RegPolicy {
            mode: RegMode::Region,
            region: Some("IN".to_string()),
            ack_operator: Some("op-1".to_string()),
            ack_at: Some("2026-07-23T00:00:00+05:30".to_string()),
        };
        let d = reg_posture_detail(&cfg, &policy);
        assert_eq!(d["posture"], MpVal::from("region"));
        assert_eq!(d["region"], MpVal::from("IN"));
        assert_eq!(d["operator_acked"], MpVal::from(true));
        assert_eq!(d["ack_operator"], MpVal::from("op-1"));
        assert_eq!(d["packet_rate_hz"], MpVal::from(150u64));
        assert_eq!(d["packet_rate_applied"], MpVal::from(true));
        assert_eq!(d["band_target"], MpVal::from("900"));
        assert_eq!(d["tx_power_target_dbm"], MpVal::from(20i64));
        assert_eq!(
            d["module_knob_mechanism"],
            MpVal::from("crsf_lua_parameter")
        );
        // Honest: the event records targets, never a module read-back.
        assert_eq!(d["module_knobs_verified"], MpVal::from(false));
    }

    #[test]
    fn detail_omits_unset_optionals_never_fabricates() {
        // Default config (no TX-power target) + default policy (no region, no
        // ack): the optional fields are ABSENT, not empty-string placeholders.
        let d = reg_posture_detail(&CrsfLaneConfig::default(), &RegPolicy::default());
        assert_eq!(d["posture"], MpVal::from("unrestricted"));
        assert!(!d.contains_key("region"));
        assert!(!d.contains_key("ack_operator"));
        assert!(!d.contains_key("ack_at"));
        assert!(!d.contains_key("tx_power_target_dbm"));
        assert_eq!(d["operator_acked"], MpVal::from(false));
        assert_eq!(d["band_target"], MpVal::from("dual"));
    }

    #[test]
    fn unacked_unrestricted_posture_warns() {
        assert_eq!(reg_posture_severity(&RegPolicy::default()), Level::Warn);
        let acked = RegPolicy {
            ack_operator: Some("op-1".to_string()),
            ..Default::default()
        };
        assert_eq!(reg_posture_severity(&acked), Level::Info);
        let pinned = RegPolicy {
            mode: RegMode::Region,
            region: Some("US".to_string()),
            ..Default::default()
        };
        assert_eq!(reg_posture_severity(&pinned), Level::Info);
    }
}
