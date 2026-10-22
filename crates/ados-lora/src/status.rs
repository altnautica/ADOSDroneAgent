//! The REST/sidecar bodies: `GET /api/v1/lora` (the `lora.json` sidecar is
//! the same object minus `present`), `GET /api/v1/lora/regions` and the
//! absent-service shapes the control surface serves when the lane is not
//! running.

use serde_json::{json, Map, Value};

use ados_lora_governor::band::{self, ModuleCaps};
use ados_lora_governor::governor::{GovernorStatus, RULE_KEYS};
use ados_lora_governor::regions::{self, elrs_domains, Plan, Resolved};

/// The `region` object of the lane body.
pub fn region_object(resolved: Option<Resolved>) -> Value {
    match resolved {
        None => Value::Null,
        Some(r) => json!({
            "code": r.plan.code,
            "name": r.plan.name,
            "plan_id": r.plan.plan_id,
            "source": r.source.as_str(),
            "band_mhz": [r.plan.band_mhz.0, r.plan.band_mhz.1],
            "citation": r.plan.citation,
            "tx": r.plan.tx,
            "meshtastic_preset": r.plan.meshtastic,
        }),
    }
}

/// The Meshtastic bridge block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshtasticState {
    pub enabled: bool,
    pub connected: bool,
    pub port: Option<String>,
    pub region_preset: Option<&'static str>,
}

/// Everything the lane body carries besides the governor.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneState {
    pub transport: &'static str,
    pub module_band: String,
    pub resolved: Option<Resolved>,
    pub node_id: u16,
    pub rx_frames: u64,
    pub rx_crc_errors: u64,
    pub meshtastic: MeshtasticState,
}

/// The `lora.json` sidecar body (the GET body minus `present`).
pub fn lane_body(lane: &LaneState, gov: &GovernorStatus) -> Value {
    json!({
        "v": crate::paths::LORA_SIDECAR_VERSION,
        "transport": lane.transport,
        "module_band": lane.module_band,
        "region": region_object(lane.resolved),
        "channel": gov.channel,
        "node_id": lane.node_id,
        "tx_allowed": gov.tx_allowed,
        "blocked_reason": gov.blocked_reason,
        "governor": gov.governor,
        "tx": gov.tx,
        "rx": {"frames": lane.rx_frames, "crc_errors": lane.rx_crc_errors},
        "meshtastic": {
            "enabled": lane.meshtastic.enabled,
            "connected": lane.meshtastic.connected,
            "port": lane.meshtastic.port,
            "region_preset": lane.meshtastic.region_preset,
        },
    })
}

fn zero_by_rule() -> Value {
    let mut m = Map::new();
    for k in RULE_KEYS {
        m.insert(k.into(), json!(0));
    }
    Value::Object(m)
}

/// `GET /api/v1/lora` when the lane service is absent: `present: false`,
/// every other field at its empty value.
pub fn absent_body() -> Value {
    json!({
        "present": false,
        "transport": null,
        "module_band": null,
        "region": null,
        "channel": null,
        "node_id": null,
        "tx_allowed": false,
        "blocked_reason": null,
        "governor": {"rules": [], "next_tx_allowed_ms": 0, "last_refusal": null},
        "tx": {"frames": 0, "refused": 0, "refused_by_rule": zero_by_rule()},
        "rx": {"frames": 0, "crc_errors": 0},
        "meshtastic": {"enabled": false, "connected": false, "port": null, "region_preset": null},
    })
}

fn region_row(p: &Plan, caps: Option<&ModuleCaps>, bw_khz: u32) -> Value {
    let (g24, sub_ghz) = elrs_domains(p.code);
    json!({
        "code": p.code,
        "name": p.name,
        "plan_id": p.plan_id,
        "band_mhz": [p.band_mhz.0, p.band_mhz.1],
        "rules": p.rules(),
        "supported_by_module": caps.map(|c| band::supported(p, c, bw_khz)),
        "citation": p.citation,
        "meshtastic_preset": p.meshtastic,
        "elrs_domains": {"2g4": g24, "900": sub_ghz},
    })
}

/// `GET /api/v1/lora/regions`: the compiled table. `caps` is the fitted
/// module (from the lane sidecar's `module_band`); `supported_by_module` is
/// null when no module is known.
pub fn regions_body(caps: Option<&ModuleCaps>, bw_khz: u32) -> Value {
    json!({
        "regions": regions::all().map(|p| region_row(p, caps, bw_khz)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_body_flags_module_support() {
        let body = regions_body(Some(&ModuleCaps::e22_900()), 125);
        let rows = body["regions"].as_array().unwrap();
        assert_eq!(rows.len(), 32);
        let row = |code: &str| rows.iter().find(|r| r["code"] == code).unwrap().clone();
        assert_eq!(row("IN")["supported_by_module"], true);
        assert_eq!(row("CN")["supported_by_module"], false);
        assert_eq!(row("JP")["supported_by_module"], false);
        assert_eq!(row("CN")["rules"], json!([]));
        assert_eq!(row("EU")["rules"], json!(["max_power", "duty_cycle"]));
        assert_eq!(
            row("EU")["elrs_domains"],
            json!({"2g4": "CE_LBT", "900": "EU868"})
        );
        let unknown = regions_body(None, 125);
        assert_eq!(unknown["regions"][0]["supported_by_module"], Value::Null);
    }

    #[test]
    fn absent_body_is_present_false_with_empty_values() {
        let b = absent_body();
        assert_eq!(b["present"], false);
        assert_eq!(b["tx_allowed"], false);
        assert_eq!(b["tx"]["refused_by_rule"]["region_unset"], 0);
        assert_eq!(b["governor"]["rules"], json!([]));
    }
}
