//! Ground-station service contexts: GNSS, LoRa and the WFB receive adapters.
//!
//! Each is `None` in [`PageContext`](crate::pages::PageContext) when its agent
//! route answers `present: false` (the service is not fitted) or does not
//! answer at all, so a page or OLED screen keyed on one is shown only when the
//! service exists.
//!
//! The LoRa governor summary (region code, highest duty use, short TX state)
//! is derived here once, by pure functions the TFT page and the OLED screen
//! both read, so the two surfaces always agree.

use embedded_graphics::pixelcolor::Rgb888;

pub use ados_protocol::radio_adapters::{AdapterRole, DiversityReport, RadioAdapterReport};

use crate::graphics::palette::Palette;

/// Severity of a derived state, mapped to the generated status tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Error,
}

impl Level {
    /// The status token for this level.
    pub fn color(self, palette: &Palette) -> Rgb888 {
        match self {
            Self::Ok => palette.status_success,
            Self::Warn => palette.status_warning,
            Self::Error => palette.status_error,
        }
    }
}

// ── GNSS ────────────────────────────────────────────────────────────

/// The RTK base block of `GET /api/v1/gnss`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GnssBaseCtx {
    /// `off`, `survey` or `fixed`.
    pub mode: Option<String>,
    /// `off`, `surveying`, `fixed` or `failed`.
    pub state: Option<String>,
    pub elapsed_s: Option<f64>,
    pub min_dur_s: Option<f64>,
    pub accuracy_m: Option<f64>,
    pub target_accuracy_m: Option<f64>,
}

impl GnssBaseCtx {
    /// Survey-in progress 0..1 (elapsed over the minimum duration).
    pub fn survey_fraction(&self) -> Option<f64> {
        match (self.elapsed_s, self.min_dur_s) {
            (Some(e), Some(m)) if m > 0.0 => Some((e / m).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

/// `GET /api/v1/gnss`: the first receiver plus the base and heading blocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GnssCtx {
    /// `none`, `2d`, `3d`, `dgps`, `rtk_float` or `rtk_fixed`; `None` when no
    /// receiver reports.
    pub fix: Option<String>,
    pub sats: Option<i64>,
    pub hdop: Option<f64>,
    pub pps_locked: Option<bool>,
    pub base: Option<GnssBaseCtx>,
    /// Dual-antenna heading in degrees, only when the heading block says it
    /// is available.
    pub heading_deg: Option<f64>,
}

/// Upper-case panel label of a wire fix value.
pub fn fix_label(fix: Option<&str>) -> &'static str {
    match fix {
        Some("none") => "NO FIX",
        Some("2d") => "2D",
        Some("3d") => "3D",
        Some("dgps") => "DGPS",
        Some("rtk_float") => "RTK-FLT",
        Some("rtk_fixed") => "RTK-FIX",
        _ => "--",
    }
}

/// Short lower-case fix for the OLED status line (`3d`, `rtk`, ...).
pub fn fix_short(fix: Option<&str>) -> &'static str {
    match fix {
        Some("none") => "none",
        Some("2d") => "2d",
        Some("3d") => "3d",
        Some("dgps") => "dgps",
        Some("rtk_float") => "flt",
        Some("rtk_fixed") => "rtk",
        _ => "--",
    }
}

// ── radio adapters ──────────────────────────────────────────────────

/// `GET /api/v1/radio/adapters`, in the shared protocol types.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RadioAdaptersCtx {
    pub adapters: Vec<RadioAdapterReport>,
    pub diversity: DiversityReport,
}

/// The strongest receive chain of an adapter, in dBm.
pub fn best_rssi(adapter: &RadioAdapterReport) -> Option<i32> {
    adapter.rssi_dbm.iter().copied().max()
}

/// An adapter's loss in percent: missed over received plus missed. `None`
/// before any frame was counted.
pub fn loss_pct(adapter: &RadioAdapterReport) -> Option<f64> {
    let total = adapter.packets + adapter.lost;
    (total > 0).then(|| adapter.lost as f64 * 100.0 / total as f64)
}

// ── LoRa ────────────────────────────────────────────────────────────

/// One `duty_cycle` rule of the active plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoraDutyRule {
    pub sub_band: Option<String>,
    pub used_pct: f64,
    pub limit_pct: f64,
}

impl LoraDutyRule {
    /// Use as a fraction of the limit (1.0 = at the limit).
    pub fn fraction(&self) -> f64 {
        if self.limit_pct > 0.0 {
            self.used_pct / self.limit_pct
        } else {
            0.0
        }
    }

    /// Meter level: under 70 % of the limit neutral, 70-99 % warning, at the
    /// limit error.
    pub fn level(&self) -> Option<Level> {
        let f = self.fraction();
        if f >= 1.0 {
            Some(Level::Error)
        } else if f >= 0.7 {
            Some(Level::Warn)
        } else {
            None
        }
    }
}

/// One received beacon (newest first in [`LoraCtx::beacons`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoraBeacon {
    pub node_id: i64,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    /// Seconds between reception and the poll that read it.
    pub age_s: Option<u64>,
}

/// `GET /api/v1/lora` plus the newest beacons.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoraCtx {
    pub region_code: Option<String>,
    pub region_name: Option<String>,
    pub plan_id: Option<String>,
    pub tx_allowed: bool,
    pub blocked_reason: Option<String>,
    /// Milliseconds until the governor allows the next transmission.
    pub next_tx_allowed_ms: u64,
    /// The plan's `duty_cycle` rules, in plan order.
    pub duty_rules: Vec<LoraDutyRule>,
    pub erp_mw: Option<f64>,
    pub max_erp_mw: Option<f64>,
    /// Transmissions the governor refused since start.
    pub refused: Option<i64>,
    /// Beacons in the receive log (the route caps it at 100).
    pub beacons_heard: usize,
    /// The newest two beacons, newest first.
    pub beacons: Vec<LoraBeacon>,
}

/// The governor's transmit state in its short panel form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoraTxState {
    Ok,
    /// Paused by the duty or dwell rule; seconds until the next allowed TX.
    Wait(u64),
    /// Listen-before-talk found the channel busy.
    Busy,
    BlockedRegion,
    BlockedBand,
    BlockedPower,
    /// Refused for a reason this panel does not name.
    Blocked,
}

impl LoraTxState {
    /// `TX OK`, `TX WAIT mm:ss`, `TX BUSY`, `TX BLK REGION|BAND|PWR`.
    pub fn short(self) -> String {
        match self {
            Self::Ok => "TX OK".to_string(),
            Self::Wait(s) => format!("TX WAIT {:02}:{:02}", s / 60, s % 60),
            Self::Busy => "TX BUSY".to_string(),
            Self::BlockedRegion => "TX BLK REGION".to_string(),
            Self::BlockedBand => "TX BLK BAND".to_string(),
            Self::BlockedPower => "TX BLK PWR".to_string(),
            Self::Blocked => "TX BLK".to_string(),
        }
    }

    /// Operator copy for the reason, `None` when transmitting is allowed.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::Wait(_) => Some("limit reached, resumes automatically"),
            Self::Busy => Some("channel busy, retrying"),
            Self::BlockedRegion => Some("region not set, receiving only"),
            Self::BlockedBand => Some("module cannot tune the region band"),
            Self::BlockedPower => Some("power over the plan limit"),
            Self::Blocked => Some("transmit refused"),
        }
    }

    pub fn level(self) -> Level {
        match self {
            Self::Ok => Level::Ok,
            Self::Wait(_) | Self::Busy => Level::Warn,
            _ => Level::Error,
        }
    }
}

/// Derive the short TX state from the governor fields. A named reason wins;
/// `tx_allowed` without one is OK, a refusal without one is a bare block.
pub fn lora_tx_state(
    tx_allowed: bool,
    blocked_reason: Option<&str>,
    next_tx_allowed_ms: u64,
) -> LoraTxState {
    match blocked_reason {
        Some("duty_exhausted" | "dwell_time") => {
            LoraTxState::Wait(next_tx_allowed_ms.div_ceil(1000))
        }
        Some("lbt_busy") => LoraTxState::Busy,
        Some("region_unset") => LoraTxState::BlockedRegion,
        Some("module_band_mismatch") => LoraTxState::BlockedBand,
        Some("power_over_limit") => LoraTxState::BlockedPower,
        Some(_) => LoraTxState::Blocked,
        None if tx_allowed => LoraTxState::Ok,
        None => LoraTxState::Blocked,
    }
}

impl LoraCtx {
    pub fn tx_state(&self) -> LoraTxState {
        lora_tx_state(
            self.tx_allowed,
            self.blocked_reason.as_deref(),
            self.next_tx_allowed_ms,
        )
    }

    /// The region code, `--` when unset.
    pub fn region_label(&self) -> &str {
        self.region_code.as_deref().unwrap_or("--")
    }

    /// The duty rule closest to its limit (first on a tie).
    pub fn top_duty(&self) -> Option<&LoraDutyRule> {
        self.duty_rules
            .iter()
            .fold(None, |best: Option<&LoraDutyRule>, r| match best {
                Some(b) if b.fraction() >= r.fraction() => Some(b),
                _ => Some(r),
            })
    }
}

/// A duty percentage in the governor's short form (`0.4`, `12`).
pub fn pct_short(pct: f64) -> String {
    if pct < 10.0 {
        format!("{pct:.1}")
    } else {
        format!("{pct:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_state_table() {
        let cases: [(bool, Option<&str>, u64, &str, Level); 9] = [
            (true, None, 0, "TX OK", Level::Ok),
            (
                false,
                Some("duty_exhausted"),
                462_000,
                "TX WAIT 07:42",
                Level::Warn,
            ),
            (
                false,
                Some("dwell_time"),
                1_500,
                "TX WAIT 00:02",
                Level::Warn,
            ),
            (false, Some("lbt_busy"), 0, "TX BUSY", Level::Warn),
            (
                false,
                Some("region_unset"),
                0,
                "TX BLK REGION",
                Level::Error,
            ),
            (
                false,
                Some("module_band_mismatch"),
                0,
                "TX BLK BAND",
                Level::Error,
            ),
            (
                false,
                Some("power_over_limit"),
                0,
                "TX BLK PWR",
                Level::Error,
            ),
            (false, Some("something_new"), 0, "TX BLK", Level::Error),
            (false, None, 0, "TX BLK", Level::Error),
        ];
        for (allowed, reason, next, short, level) in cases {
            let s = lora_tx_state(allowed, reason, next);
            assert_eq!(s.short(), short, "{reason:?}");
            assert_eq!(s.level(), level, "{reason:?}");
        }
    }

    #[test]
    fn top_duty_is_the_rule_nearest_its_limit() {
        let ctx = LoraCtx {
            duty_rules: vec![
                LoraDutyRule {
                    sub_band: Some("a".into()),
                    used_pct: 0.81,
                    limit_pct: 1.0,
                },
                LoraDutyRule {
                    sub_band: Some("b".into()),
                    used_pct: 3.0,
                    limit_pct: 10.0,
                },
            ],
            ..LoraCtx::default()
        };
        assert_eq!(ctx.top_duty().unwrap().sub_band.as_deref(), Some("a"));
        assert_eq!(ctx.top_duty().unwrap().level(), Some(Level::Warn));
        assert_eq!(LoraCtx::default().top_duty(), None);
        assert_eq!(LoraCtx::default().region_label(), "--");
    }

    #[test]
    fn duty_levels_follow_the_thresholds() {
        let r = |used| LoraDutyRule {
            sub_band: None,
            used_pct: used,
            limit_pct: 1.0,
        };
        assert_eq!(r(0.69).level(), None);
        assert_eq!(r(0.7).level(), Some(Level::Warn));
        assert_eq!(r(1.0).level(), Some(Level::Error));
    }
}
