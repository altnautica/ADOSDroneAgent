//! The Contract E sidecar: `/run/ados/crsf-stats.json`.
//!
//! Exactly this field set, snake_case, written atomically ~1 Hz while the
//! service runs and once on every degraded-state entry:
//!
//! ```json
//! { "v": 1, "state": "…", "rssi_dbm": …, "lq_uplink": …, "lq_downlink": …,
//!   "snr_db": …, "band": …, "packet_rate_hz": …, "tx_power_dbm": …,
//!   "tx_frames_per_s": …, "rx_frames_per_s": …, "rf_unverified": …,
//!   "flyable": …, "mode": …, "channel_source": …, "relay_role": … }
//! ```
//!
//! `state` ∈ `unconfigured|ready|link_ok|degraded|rf_unverified|disabled`.
//! Every number/string is `null` when unmeasured, and `rf_unverified` is a
//! tri-state boolean whose `null` means "no verdict yet" — a reading is
//! reported only when it is real, never fabricated. `flyable` is a strict
//! boolean gate: true ONLY for a state with a received-side proof (`link_ok`
//! / `degraded`) — an unproven lane is conservatively not flyable, never
//! "unknown so maybe".

use serde_json::{json, Value};

use crate::link::LaneState;
use crate::paths::{run_path, write_sidecar, CRSF_STATS_SIDECAR_VERSION};
use crate::telemetry::LinkStatistics;

/// Fold a wire RSSI byte into physical dBm. The field is a signed dBm where
/// some modules transmit the magnitude instead (an RSSI of −51 dBm sent as
/// 51); a positive received-power reading on an RC link is physically
/// implausible, so a positive value is folded to its negative counterpart. A
/// zero reads zero (no signal figure), which the caller should treat as
/// unmeasured rather than a real power.
pub fn rssi_dbm_from_wire(raw: i8) -> i64 {
    if raw > 0 {
        -i64::from(raw)
    } else {
        i64::from(raw)
    }
}

/// The measured values a sidecar write reports. Every field is optional:
/// `None` serializes as `null` — unmeasured, never fabricated.
#[derive(Debug, Clone, Default)]
pub struct StatsInputs<'a> {
    /// Last decoded link statistics, when fresh enough to report.
    pub link: Option<&'a LinkStatistics>,
    /// The operating band, when known. Not measurable from link statistics;
    /// stays `None` until a real source (a parameter read) supplies it.
    pub band: Option<&'a str>,
    /// The configured RC frame cadence while the transmitter runs.
    pub packet_rate_hz: Option<u16>,
    /// Measured transmitted frames per second over the last interval.
    pub tx_frames_per_s: Option<f64>,
    /// Measured received frames per second over the last interval.
    pub rx_frames_per_s: Option<f64>,
    /// The configured lane mode (`crsf_rc` while the RC channel lane runs;
    /// `mavlink` / `airport` while the lane stands aside for that mode's
    /// owner). In `mavlink` mode the MAVLink router owns the carrier and, by
    /// default, runs it telemetry-only — the host->FC command-down direction
    /// is gated closed until `radio.crsf.mavlink_command_enabled` is set — so
    /// this mode never denotes a live bidirectional command lane.
    pub mode: Option<&'a str>,
    /// Where the transmitted channels come from, once a source has injected.
    pub channel_source: Option<&'a str>,
    /// The relay role, when this node participates in a relay chain.
    pub relay_role: Option<&'a str>,
}

/// Build the sidecar body — the exact pinned field set, nothing more.
pub fn build_stats_value(state: LaneState, inputs: &StatsInputs<'_>) -> Value {
    let rssi_dbm = inputs
        .link
        .map(|l| Value::from(rssi_dbm_from_wire(l.active_uplink_rssi())))
        .unwrap_or(Value::Null);
    let lq_uplink = inputs
        .link
        .map(|l| Value::from(l.uplink_lq))
        .unwrap_or(Value::Null);
    let lq_downlink = inputs
        .link
        .map(|l| Value::from(l.downlink_lq))
        .unwrap_or(Value::Null);
    let snr_db = inputs
        .link
        .map(|l| Value::from(l.uplink_snr))
        .unwrap_or(Value::Null);
    let tx_power_dbm = inputs
        .link
        .map(|l| Value::from(l.uplink_tx_power))
        .unwrap_or(Value::Null);
    json!({
        "v": CRSF_STATS_SIDECAR_VERSION,
        "state": state.as_str(),
        "rssi_dbm": rssi_dbm,
        "lq_uplink": lq_uplink,
        "lq_downlink": lq_downlink,
        "snr_db": snr_db,
        "band": inputs.band,
        "packet_rate_hz": inputs.packet_rate_hz,
        "tx_power_dbm": tx_power_dbm,
        "tx_frames_per_s": inputs.tx_frames_per_s,
        "rx_frames_per_s": inputs.rx_frames_per_s,
        "rf_unverified": state.rf_unverified_flag(),
        "flyable": state.flyable(),
        "mode": inputs.mode,
        "channel_source": inputs.channel_source,
        "relay_role": inputs.relay_role,
    })
}

/// Build + write the sidecar atomically, mirroring the identical body into
/// the logging store as a `link.crsf_status` event so the reading survives a
/// reboot and is queryable offline. Best-effort on both surfaces: an absent
/// run dir or logging daemon never disturbs the lane.
pub fn write_stats_sidecar(
    state: LaneState,
    inputs: &StatsInputs<'_>,
    metrics: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let v = build_stats_value(state, inputs);
    let path = run_path("crsf-stats.json");
    let _ = write_sidecar(&path, &v);
    if let Some(metrics) = metrics {
        metrics.emit_event(
            "link.crsf_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

/// Convert a JSON object into the logging store's open detail map, so the
/// full status body rides a single event. Numbers preserve their
/// integer-vs-float kind and null round-trips to nil.
fn json_object_to_fields(value: &Value) -> ados_protocol::logd::Fields {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_mpv(v)))
            .collect(),
        _ => ados_protocol::logd::Fields::new(),
    }
}

fn json_to_mpv(value: &Value) -> ados_protocol::logd::Value {
    use ados_protocol::logd::Value as MpVal;
    match value {
        Value::Null => MpVal::Nil,
        Value::Bool(b) => MpVal::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MpVal::from(i)
            } else if let Some(u) = n.as_u64() {
                MpVal::from(u)
            } else {
                MpVal::from(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => MpVal::from(s.as_str()),
        Value::Array(items) => MpVal::Array(items.iter().map(json_to_mpv).collect()),
        Value::Object(map) => MpVal::Map(
            map.iter()
                .map(|(k, v)| (MpVal::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned field set, in one place, so a drift in either direction
    /// (a missing field or an invented one) fails loudly.
    const PINNED_FIELDS: [&str; 16] = [
        "v",
        "state",
        "rssi_dbm",
        "lq_uplink",
        "lq_downlink",
        "snr_db",
        "band",
        "packet_rate_hz",
        "tx_power_dbm",
        "tx_frames_per_s",
        "rx_frames_per_s",
        "rf_unverified",
        "flyable",
        "mode",
        "channel_source",
        "relay_role",
    ];

    fn sample_link() -> LinkStatistics {
        LinkStatistics {
            uplink_rssi_ant1: -51,
            uplink_rssi_ant2: -60,
            uplink_lq: 99,
            uplink_snr: 8,
            active_antenna: 0,
            rf_mode: 4,
            uplink_tx_power: 20,
            downlink_rssi: -55,
            downlink_lq: 97,
            downlink_snr: 6,
        }
    }

    #[test]
    fn body_carries_exactly_the_pinned_fields() {
        for state in [
            LaneState::Disabled,
            LaneState::Unconfigured,
            LaneState::Ready,
            LaneState::LinkOk,
            LaneState::Degraded,
            LaneState::RfUnverified,
        ] {
            let v = build_stats_value(state, &StatsInputs::default());
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), PINNED_FIELDS.len(), "state {state:?}");
            for field in PINNED_FIELDS {
                assert!(obj.contains_key(field), "missing {field} in {state:?}");
            }
        }
    }

    #[test]
    fn disabled_body_is_all_null_except_version_state_and_flyable() {
        let v = build_stats_value(LaneState::Disabled, &StatsInputs::default());
        assert_eq!(v["v"], 1);
        assert_eq!(v["state"], "disabled");
        // The flyable gate is a strict boolean, never null: unproven reads
        // false, conservatively.
        assert_eq!(v["flyable"], false);
        for field in PINNED_FIELDS {
            if field != "v" && field != "state" && field != "flyable" {
                assert!(v[field].is_null(), "{field} must be null when unmeasured");
            }
        }
    }

    #[test]
    fn link_ok_body_reports_the_measured_values() {
        let link = sample_link();
        let inputs = StatsInputs {
            link: Some(&link),
            band: None,
            packet_rate_hz: Some(50),
            tx_frames_per_s: Some(49.8),
            rx_frames_per_s: Some(12.0),
            mode: Some("rc"),
            channel_source: Some("inject"),
            relay_role: None,
        };
        let v = build_stats_value(LaneState::LinkOk, &inputs);
        assert_eq!(v["state"], "link_ok");
        assert_eq!(v["rssi_dbm"], -51);
        assert_eq!(v["lq_uplink"], 99);
        assert_eq!(v["lq_downlink"], 97);
        assert_eq!(v["snr_db"], 8);
        assert_eq!(v["tx_power_dbm"], 20);
        assert_eq!(v["packet_rate_hz"], 50);
        assert_eq!(v["tx_frames_per_s"], 49.8);
        assert_eq!(v["rx_frames_per_s"], 12.0);
        assert_eq!(v["rf_unverified"], false);
        assert_eq!(v["flyable"], true);
        assert_eq!(v["mode"], "rc");
        assert_eq!(v["channel_source"], "inject");
        assert!(v["band"].is_null());
        assert!(v["relay_role"].is_null());
    }

    #[test]
    fn rf_unverified_state_reports_true_and_never_flyable() {
        let v = build_stats_value(LaneState::RfUnverified, &StatsInputs::default());
        assert_eq!(v["state"], "rf_unverified");
        assert_eq!(v["rf_unverified"], true);
        assert_eq!(v["flyable"], false, "an unproven lane never reads flyable");
        // Ready = no verdict yet: null rf_unverified, still not flyable.
        let v = build_stats_value(LaneState::Ready, &StatsInputs::default());
        assert!(v["rf_unverified"].is_null());
        assert_eq!(v["flyable"], false);
    }

    #[test]
    fn only_proven_states_read_flyable_on_the_sidecar() {
        for (state, flyable) in [
            (LaneState::Disabled, false),
            (LaneState::Unconfigured, false),
            (LaneState::Ready, false),
            (LaneState::LinkOk, true),
            (LaneState::Degraded, true),
            (LaneState::RfUnverified, false),
        ] {
            let v = build_stats_value(state, &StatsInputs::default());
            assert_eq!(v["flyable"], flyable, "state {state:?}");
        }
    }

    #[test]
    fn positive_wire_rssi_folds_to_negative_dbm() {
        assert_eq!(rssi_dbm_from_wire(-51), -51);
        assert_eq!(rssi_dbm_from_wire(51), -51);
        assert_eq!(rssi_dbm_from_wire(0), 0);
        // The fold applies through the body builder too.
        let mut link = sample_link();
        link.uplink_rssi_ant1 = 51;
        let inputs = StatsInputs {
            link: Some(&link),
            ..Default::default()
        };
        let v = build_stats_value(LaneState::LinkOk, &inputs);
        assert_eq!(v["rssi_dbm"], -51);
    }

    #[test]
    fn active_antenna_selects_the_reported_rssi() {
        let mut link = sample_link();
        link.active_antenna = 1;
        let inputs = StatsInputs {
            link: Some(&link),
            ..Default::default()
        };
        let v = build_stats_value(LaneState::LinkOk, &inputs);
        assert_eq!(v["rssi_dbm"], -60);
    }

    #[test]
    fn write_stats_sidecar_lands_the_exact_body_on_disk() {
        let _g = crate::paths::test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let link = sample_link();
        let inputs = StatsInputs {
            link: Some(&link),
            packet_rate_hz: Some(50),
            mode: Some("rc"),
            ..Default::default()
        };
        write_stats_sidecar(LaneState::LinkOk, &inputs, None);
        let body: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("crsf-stats.json")).unwrap(),
        )
        .unwrap();
        std::env::remove_var("ADOS_RUN_DIR");
        assert_eq!(body, build_stats_value(LaneState::LinkOk, &inputs));
    }
}
