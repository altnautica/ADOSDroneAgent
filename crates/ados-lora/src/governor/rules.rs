//! The `dwell_time` and `duty_cycle` checks over the ledger, and the wait
//! until each would admit a frame.

use crate::ledger::{Ledger, Spend};
use crate::regions::{DutyRule, DwellRule, SubBand};

const MS: i64 = 1_000_000;
const S: i64 = 1_000_000_000;

fn same_channel(s: &Spend, mhz: f64) -> bool {
    (s.channel_mhz - mhz).abs() < 1e-6
}

/// Time until `airtime_us` more on `mhz` passes the dwell rule (0 = now).
/// `None` when no wait helps (the frame is longer than one transmission may be).
pub fn dwell_wait_ns(
    ledger: &Ledger,
    rule: &DwellRule,
    mhz: f64,
    airtime_us: u64,
    now_ns: i64,
) -> Option<i64> {
    if airtime_us > u64::from(rule.single_tx_max_ms) * 1000 {
        return None;
    }
    let on_ch: Vec<&Spend> = ledger
        .spends()
        .iter()
        .filter(|s| same_channel(s, mhz))
        .collect();
    let mut wait = 0i64;
    if let Some(last) = on_ch.last() {
        let end = last.at_ns + last.airtime_us as i64 * 1000;
        wait = wait.max(end + i64::from(rule.pause_ms) * MS - now_ns);
    }
    if let (Some(limit_ms), Some(per_s)) = (rule.limit_ms, rule.per_s) {
        let limit_us = u64::from(limit_ms) * 1000;
        if airtime_us > limit_us {
            return None;
        }
        let window = i64::from(per_s) * S;
        let in_window: Vec<&&Spend> = on_ch.iter().filter(|s| s.at_ns > now_ns - window).collect();
        let used: u64 = in_window.iter().map(|s| s.airtime_us).sum();
        if used + airtime_us > limit_us {
            let mut excess = used + airtime_us - limit_us;
            for s in in_window {
                let ages_out = s.at_ns + window - now_ns;
                if s.airtime_us >= excess {
                    wait = wait.max(ages_out);
                    break;
                }
                excess -= s.airtime_us;
            }
        }
    }
    Some(wait.max(0))
}

/// Charged airtime in `sb` over the rule's window, in microseconds. A radio
/// that reports its own transmit utilisation (which includes rebroadcasts the
/// lane did not originate) counts whichever is larger.
pub fn duty_used_us(
    ledger: &Ledger,
    sb: &SubBand,
    rule: &DutyRule,
    now_ns: i64,
    reported_pct: Option<f64>,
) -> u64 {
    let since = now_ns - i64::from(rule.window_s) * S;
    let own = ledger.spent_in(sb.lo_mhz, sb.hi_mhz, since);
    let reported = reported_pct.map_or(0, |pct| {
        (pct / 100.0 * f64::from(rule.window_s) * 1_000_000.0).round() as u64
    });
    own.max(reported)
}

/// Time until `airtime_us` more fits the duty budget of `sb` (0 = now).
/// `None` when the frame alone is over the budget.
pub fn duty_wait_ns(
    ledger: &Ledger,
    sb: &SubBand,
    rule: &DutyRule,
    airtime_us: u64,
    now_ns: i64,
    reported_pct: Option<f64>,
) -> Option<i64> {
    let budget = rule.budget_us();
    if airtime_us > budget {
        return None;
    }
    let mut wait = ledger
        .exhausted_until_ns()
        .map_or(0, |until| until - now_ns)
        .max(0);
    let used = duty_used_us(ledger, sb, rule, now_ns, reported_pct);
    if used + airtime_us <= budget {
        return Some(wait);
    }
    let window = i64::from(rule.window_s) * S;
    let since = now_ns - window;
    let mut excess = used + airtime_us - budget;
    let own: Vec<&Spend> = ledger
        .spends()
        .iter()
        .filter(|s| s.at_ns > since && s.channel_mhz >= sb.lo_mhz && s.channel_mhz <= sb.hi_mhz)
        .collect();
    for s in own {
        if s.airtime_us >= excess {
            wait = wait.max(s.at_ns + window - now_ns);
            return Some(wait.max(0));
        }
        excess -= s.airtime_us;
    }
    // A reported utilisation above the own ledger has no entries to age out:
    // wait one full window.
    Some(wait.max(window))
}
