//! Ground-station profile lines: radios table, GNSS summary, LoRa governor.

use super::profile::{mmss, pct, ratio_level, Level, ProfileRoutes, RowLine};
use serde_json::Value;

pub(super) fn gs_lines(routes: &ProfileRoutes) -> Vec<RowLine> {
    let mut lines = Vec::new();
    if let Some(radio) = &routes.radio {
        lines.extend(radio_lines(radio));
    }
    if let Some(gnss) = &routes.gnss {
        lines.push(gnss_line(gnss));
    }
    if let Some(lora) = &routes.lora {
        lines.push(lora_line(lora));
    }
    lines
}

/// One row per adapter: `wlan1  rxtx  -48 dBm  loss 0.1 %`.
fn radio_lines(radio: &Value) -> Vec<RowLine> {
    let Some(adapters) = radio.get("adapters").and_then(Value::as_array) else {
        return Vec::new();
    };
    adapters
        .iter()
        .map(|a| {
            let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
            let mut line = RowLine::default();
            line.push(format!("{:<8}", s("iface")), Level::Value);
            line.push(format!("{:<6}", s("role")), Level::Label);
            let rssi = a
                .get("rssi_dbm")
                .and_then(Value::as_array)
                .and_then(|v| v.iter().filter_map(Value::as_i64).max());
            match rssi {
                Some(r) => line.push(format!("{:>4} dBm  ", r), Level::Value),
                None => line.push("  -- dBm  ", Level::Label),
            }
            let packets = a.get("packets").and_then(Value::as_f64);
            let lost = a.get("lost").and_then(Value::as_f64);
            line.push("loss ", Level::Label);
            match (packets, lost) {
                (Some(p), Some(l)) if p + l > 0.0 => {
                    let loss = l / (p + l) * 100.0;
                    let level = match loss {
                        x if x >= 10.0 => Level::Error,
                        x if x >= 2.0 => Level::Warn,
                        _ => Level::Value,
                    };
                    line.push(format!("{} %", pct(loss)), level);
                }
                _ => line.push("--", Level::Label),
            }
            line
        })
        .collect()
}

/// `GNSS 3d 14 sats base survey` from the first receiver and the base block.
fn gnss_line(gnss: &Value) -> RowLine {
    let mut line = RowLine::default();
    line.push("GNSS ", Level::Label);
    let rx = gnss
        .get("receivers")
        .and_then(Value::as_array)
        .and_then(|r| r.first());
    match rx {
        Some(rx) => {
            let fix = rx.get("fix").and_then(Value::as_str).unwrap_or("none");
            let level = if fix == "none" {
                Level::Warn
            } else {
                Level::Value
            };
            line.push(fix.to_string(), level);
            if let Some(sats) = rx.get("sats").and_then(Value::as_i64) {
                line.push(format!(" {sats} sats"), Level::Value);
            }
        }
        None => line.push("no receiver", Level::Warn),
    }
    if let Some(mode) = gnss.pointer("/base/mode").and_then(Value::as_str) {
        line.push(" base ", Level::Label);
        line.push(mode.to_string(), Level::Value);
    }
    line
}

/// The LoRa governor line, e.g. `LoRa IN865 duty 0.4%/1% pwr 25/25mW TX ok  refused 0`.
pub(super) fn lora_line(lora: &Value) -> RowLine {
    let mut line = RowLine::default();
    let plan = lora
        .get("region")
        .filter(|r| !r.is_null())
        .and_then(|r| r.get("plan_id").or_else(|| r.get("code")))
        .and_then(Value::as_str)
        .unwrap_or("--");
    line.push("LoRa ", Level::Label);
    line.push(format!("{plan:<5} "), Level::Value);

    let allowed = lora.get("tx_allowed").and_then(Value::as_bool) == Some(true);
    let reason = lora.get("blocked_reason").and_then(Value::as_str);
    if !allowed && matches!(reason, Some("region_unset" | "module_band_mismatch") | None) {
        let reason = reason.unwrap_or("unknown");
        line.push(format!("TX BLOCKED {reason}"), Level::Error);
        if reason == "region_unset" {
            line.push(" (RX on)", Level::Label);
        }
        return line;
    }

    let rules = lora
        .pointer("/governor/rules")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut duty: Vec<(&Value, f64)> = rules
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("duty_cycle"))
        .filter_map(|r| {
            let used = r.get("used_pct")?.as_f64()?;
            let limit = r.get("limit_pct")?.as_f64().filter(|l| *l > 0.0)?;
            Some((r, used / limit))
        })
        .collect();
    duty.sort_by(|a, b| b.1.total_cmp(&a.1));
    let multi = duty.len() > 1;
    for (rule, ratio) in duty.iter().take(2) {
        let used = rule["used_pct"].as_f64().unwrap_or_default();
        let limit = rule["limit_pct"].as_f64().unwrap_or_default();
        let label = if multi {
            rule.get("sub_band")
                .and_then(Value::as_str)
                .unwrap_or("duty")
        } else {
            "duty"
        };
        let exhausted = rule.get("exhausted").and_then(Value::as_bool) == Some(true);
        line.push(format!("{label} "), Level::Label);
        let level = if exhausted {
            Level::Error
        } else {
            ratio_level(*ratio)
        };
        line.push(format!("{}%/{}% ", pct(used), pct(limit)), level);
    }
    if let Some(power) = rules
        .iter()
        .find(|r| r.get("kind").and_then(Value::as_str) == Some("max_power"))
    {
        if let (Some(cfg), Some(lim)) = (
            power.get("configured_mw").and_then(Value::as_f64),
            power.get("limit_mw").and_then(Value::as_f64),
        ) {
            let level = if power.get("ok").and_then(Value::as_bool) == Some(false) {
                Level::Error
            } else {
                Level::Value
            };
            line.push("pwr ", Level::Label);
            line.push(format!("{}/{}mW ", pct(cfg), pct(lim)), level);
        }
    }

    match (allowed, reason) {
        (true, _) => line.push("TX ok", Level::Value),
        (false, Some(r @ ("duty_exhausted" | "dwell_time" | "lbt_busy"))) => {
            let mut text = format!("TX WAIT {r}");
            if let Some(ms) = lora
                .pointer("/governor/next_tx_allowed_ms")
                .and_then(Value::as_f64)
                .filter(|ms| *ms > 0.0)
            {
                text = format!("{text} {}", mmss(ms / 1000.0));
            }
            line.push(text, Level::Error);
        }
        (false, r) => line.push(
            format!("TX BLOCKED {}", r.unwrap_or("unknown")),
            Level::Error,
        ),
    }

    if let Some(refused) = lora.pointer("/tx/refused").and_then(Value::as_i64) {
        line.push("  refused ", Level::Label);
        let level = if refused > 0 {
            Level::Warn
        } else {
            Level::Value
        };
        line.push(refused.to_string(), level);
        if refused > 0 {
            if let Some(rule) = lora
                .pointer("/governor/last_refusal/rule")
                .and_then(Value::as_str)
            {
                let short = if rule == "duty_cycle" { "duty" } else { rule };
                line.push(format!(" ({short})"), Level::Label);
            }
        }
    }
    line
}
