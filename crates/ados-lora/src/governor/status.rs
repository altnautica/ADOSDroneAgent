//! The governor's part of the `GET /api/v1/lora` body.

use serde_json::{json, Map, Value};

use super::power::dbm_to_mw;
use super::{rules, Governor, RULE_KEYS};
use crate::ledger::Now;
use crate::regions::PowerRef;

/// The `channel`, `governor`, `tx` blocks and the TX verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct GovernorStatus {
    pub tx_allowed: bool,
    pub blocked_reason: Option<&'static str>,
    pub channel: Value,
    pub governor: Value,
    pub tx: Value,
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

impl Governor {
    /// Status for a frame of `len` bytes (the beacon size) at `now`.
    pub fn status(&self, len: usize, now: Now) -> GovernorStatus {
        let plan = self.plan();
        let bw = self.setup.modem.bw_khz;
        let airtime = self.charge_us(len);
        let channel_mhz = self.current_channel();
        let sb = plan
            .zip(channel_mhz)
            .and_then(|(p, f)| p.sub_band_for(f, bw));
        let ant = self.setup.antenna;
        let step = sb.and_then(|s| self.power_for(s));
        let max_erp_mw = sb.and_then(|s| s.power).map(|rule| {
            let cap = super::power::conducted_cap_dbm(&rule, &ant, bw);
            round(dbm_to_mw(ant.erp_of(cap)), 2)
        });
        let channel = json!({
            "freq_mhz": channel_mhz,
            "bw_khz": bw,
            "sf": self.setup.modem.sf,
            "erp_mw": step.map(|s| round(dbm_to_mw(ant.erp_of(f64::from(s))), 2)),
            "max_erp_mw": max_erp_mw,
        });

        let mut rules_out = Vec::new();
        if let Some(p) = plan.filter(|p| p.tx) {
            if let Some(rule) = sb.and_then(|s| s.power) {
                let configured = self.setup.erp_mw.unwrap_or(match rule.reference {
                    PowerRef::Erp => rule.limit_mw,
                    _ => max_erp_mw.unwrap_or(0.0),
                });
                rules_out.push(json!({
                    "kind": "max_power",
                    "limit_mw": rule.limit_mw,
                    "reference": rule.reference,
                    "configured_mw": configured,
                    "ok": step.is_some() && max_erp_mw.is_none_or(|m| configured <= m + 1e-6),
                }));
            }
            if let Some(rule) = p.dwell {
                let ok = channel_mhz.is_some_and(|f| {
                    rules::dwell_wait_ns(&self.ledger, &rule, f, airtime, now.mono_ns) == Some(0)
                });
                rules_out.push(json!({
                    "kind": "dwell_time",
                    "limit_ms": rule.limit_ms.unwrap_or(rule.single_tx_max_ms),
                    "per_s": rule.per_s,
                    "single_tx_max_ms": rule.single_tx_max_ms,
                    "pause_ms": rule.pause_ms,
                    "last_tx_ms": self.last_grant.as_ref().map(|g| g.airtime_us.div_ceil(1000)),
                    "ok": ok,
                }));
            }
            for s in p.sub_bands {
                let Some(rule) = s.duty else { continue };
                let used = rules::duty_used_us(
                    &self.ledger,
                    s,
                    &rule,
                    now.mono_ns,
                    self.reported_air_util_pct,
                );
                let budget = rule.budget_us();
                let exhausted = rules::duty_wait_ns(
                    &self.ledger,
                    s,
                    &rule,
                    airtime,
                    now.mono_ns,
                    self.reported_air_util_pct,
                ) != Some(0);
                rules_out.push(json!({
                    "kind": "duty_cycle",
                    "window_s": rule.window_s,
                    "limit_pct": rule.limit_pct,
                    "used_pct": round(used as f64 / (f64::from(rule.window_s) * 1e6) * 100.0, 3),
                    "airtime_ms_window": used / 1000,
                    "budget_ms_window": budget / 1000,
                    "exhausted": exhausted,
                    "sub_band": s.label(),
                }));
            }
            if let Some(rule) = p.lbt {
                rules_out.push(json!({
                    "kind": "lbt",
                    "threshold_dbm": rule.threshold_dbm,
                    "listen_us": rule.listen_us,
                    "last_rssi_dbm": self.lbt_last_rssi,
                    "busy_count": self.lbt_busy_count,
                }));
            }
        }
        let governor = json!({
            "rules": rules_out,
            "next_tx_allowed_ms": self.next_tx_allowed_ms(len, now),
            "last_refusal": self.last_refusal.map(|(r, at)| json!({"rule": r.rule(), "at_ms": at})),
            "airtime_charge_ms": round(airtime as f64 / 1000.0, 1),
            "apc": plan.is_some_and(|p| p.apc).then(|| json!({"steps_down": self.apc.steps_down})),
        });
        let mut by_rule = Map::new();
        for k in RULE_KEYS {
            by_rule.insert(
                k.into(),
                json!(self.counters.by_rule.get(k).copied().unwrap_or(0)),
            );
        }
        let tx = json!({
            "frames": self.counters.frames,
            "refused": self.counters.refused,
            "refused_by_rule": by_rule,
        });
        GovernorStatus {
            tx_allowed: self.tx_allowed(),
            blocked_reason: self.static_block.or(self.blocked).map(|r| r.reason()),
            channel,
            governor,
            tx,
        }
    }
}
