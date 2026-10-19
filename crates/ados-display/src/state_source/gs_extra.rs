//! Ground-station service reads for the GNSS, radios and LoRa surfaces: the
//! `/api/v1/gnss`, `/api/v1/radio/adapters`, `/api/v1/lora` and
//! `/api/v1/lora/beacons` routes. Each read is independent; a failed read or a
//! `present: false` body leaves its context `None`.

use std::time::{SystemTime, UNIX_EPOCH};

use ados_protocol::radio_adapters::RadioAdaptersBody;

use super::*;
use crate::pages::gs_ctx::{
    GnssBaseCtx, GnssCtx, LoraBeacon, LoraCtx, LoraDutyRule, RadioAdaptersCtx,
};

/// How many beacons the LoRa surfaces list.
const BEACONS_SHOWN: usize = 2;

impl StateSource {
    /// Fetch the GNSS, radio-adapter and LoRa routes into `ctx`.
    pub(super) fn apply_gs_extras(&self, ctx: &mut PageContext) {
        ctx.gnss = gnss_ctx(self.get_json("/api/v1/gnss").as_ref());
        ctx.radio_adapters = radio_adapters_ctx(self.get_json("/api/v1/radio/adapters").as_ref());
        let lora = self.get_json("/api/v1/lora");
        // The beacon log is read only when a LoRa service is fitted.
        ctx.lora = present(lora.as_ref()).and_then(|l| {
            lora_ctx(
                Some(l),
                self.get_json("/api/v1/lora/beacons").as_ref(),
                now_ms(),
            )
        });
    }
}

/// Wall-clock milliseconds since the Unix epoch (the beacon timestamp base).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The body when it is an object not marked `present: false`.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| v.is_object() && v.get("present").and_then(Value::as_bool) != Some(false))
}

/// A child that is a JSON object (null and absent both read `None`).
fn obj<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|c| c.is_object())
}

/// `GET /api/v1/gnss` → the first receiver plus base and heading.
pub(super) fn gnss_ctx(v: Option<&Value>) -> Option<GnssCtx> {
    let v = present(v)?;
    let rx = v
        .get("receivers")
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    let heading = obj(v, "heading")
        .filter(|h| bool_field(h, "available"))
        .and_then(|h| f64_field(h, "deg"));
    Some(GnssCtx {
        fix: rx.and_then(|r| string_field(r, "fix")),
        sats: rx.and_then(|r| i64_field(r, "sats")),
        hdop: rx.and_then(|r| f64_field(r, "hdop")),
        pps_locked: rx
            .and_then(|r| r.get("pps_locked"))
            .and_then(Value::as_bool),
        base: obj(v, "base").map(|b| GnssBaseCtx {
            mode: string_field(b, "mode"),
            state: string_field(b, "state"),
            elapsed_s: f64_field(b, "elapsed_s"),
            min_dur_s: f64_field(b, "min_dur_s"),
            accuracy_m: f64_field(b, "accuracy_m"),
            target_accuracy_m: f64_field(b, "target_accuracy_m"),
        }),
        heading_deg: heading,
    })
}

/// `GET /api/v1/radio/adapters`, decoded into the shared protocol body.
pub(super) fn radio_adapters_ctx(v: Option<&Value>) -> Option<RadioAdaptersCtx> {
    let body: RadioAdaptersBody = serde_json::from_value(v?.clone()).ok()?;
    body.present.then_some(RadioAdaptersCtx {
        adapters: body.adapters,
        diversity: body.diversity,
    })
}

/// `GET /api/v1/lora` plus `GET /api/v1/lora/beacons` (newest first), with
/// each beacon's age taken against `now_ms`.
pub(super) fn lora_ctx(
    lora: Option<&Value>,
    beacons: Option<&Value>,
    now_ms: u64,
) -> Option<LoraCtx> {
    let v = present(lora)?;
    let region = obj(v, "region");
    let channel = obj(v, "channel");
    let governor = obj(v, "governor");
    let duty_rules = governor
        .and_then(|g| g.get("rules"))
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter(|r| r.get("kind").and_then(Value::as_str) == Some("duty_cycle"))
                .map(|r| LoraDutyRule {
                    sub_band: string_field(r, "sub_band"),
                    used_pct: f64_field(r, "used_pct").unwrap_or(0.0),
                    limit_pct: f64_field(r, "limit_pct").unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default();
    let beacon_list = beacons
        .and_then(|b| b.get("beacons"))
        .and_then(Value::as_array);
    Some(LoraCtx {
        region_code: region.and_then(|r| string_field(r, "code")),
        region_name: region.and_then(|r| string_field(r, "name")),
        plan_id: region.and_then(|r| string_field(r, "plan_id")),
        tx_allowed: bool_field(v, "tx_allowed"),
        blocked_reason: string_field(v, "blocked_reason"),
        next_tx_allowed_ms: governor
            .and_then(|g| g.get("next_tx_allowed_ms"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        duty_rules,
        erp_mw: channel.and_then(|c| f64_field(c, "erp_mw")),
        max_erp_mw: channel.and_then(|c| f64_field(c, "max_erp_mw")),
        refused: obj(v, "tx").and_then(|t| i64_field(t, "refused")),
        beacons_heard: beacon_list.map_or(0, Vec::len),
        beacons: beacon_list
            .map(|list| {
                list.iter()
                    .take(BEACONS_SHOWN)
                    .map(|b| LoraBeacon {
                        node_id: i64_field(b, "node_id").unwrap_or(0),
                        rssi_dbm: f64_field(b, "rssi_dbm"),
                        snr_db: f64_field(b, "snr_db"),
                        age_s: b
                            .get("received_at_ms")
                            .and_then(Value::as_u64)
                            .map(|at| now_ms.saturating_sub(at) / 1000),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests;
