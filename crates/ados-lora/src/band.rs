//! What the fitted radio can tune, and whether that meets a plan.
//!
//! A plan is supported by the module when at least one tunable channel has its
//! whole occupied bandwidth inside a plan sub-band, on the plan's channel grid
//! where the plan has one, the module offers at least the plan's minimum
//! hop-channel count, and, for a plan that requires listen-before-talk, the
//! host can measure ambient RSSI through it. Otherwise transmit is blocked
//! with `module_band_mismatch`: a hardware fact no amount of waiting clears.

use crate::regions::Plan;

/// How the module selects a frequency.
#[derive(Debug, Clone, PartialEq)]
pub enum Tuning {
    /// Fixed channels at `start_mhz + n * step_mhz`, `n` in `0..count`.
    Grid {
        /// First channel centre, MHz.
        start_mhz: f64,
        /// Channel spacing, MHz.
        step_mhz: f64,
        /// Number of channels.
        count: u32,
    },
    /// Any centre in `lo_mhz..=hi_mhz`. `hops` is whether the transport can
    /// change channel per frame (a Meshtastic node sits on one preset slot).
    Continuous {
        /// Lowest tunable frequency, MHz.
        lo_mhz: f64,
        /// Highest tunable frequency, MHz.
        hi_mhz: f64,
        /// Whether the transport can change channel per frame.
        hops: bool,
    },
}

/// The fitted radio's tuning range and power steps.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleCaps {
    /// The band label reported as `module_band` (`850-930`, `410-493`,
    /// `863-928`, `any`).
    pub band: String,
    /// How the radio tunes.
    pub tuning: Tuning,
    /// Conducted output steps the module can be set to, in dBm, descending.
    pub power_steps_dbm: Vec<i8>,
    /// Whether the host can sample ambient RSSI through the transport, which
    /// listen-before-talk needs.
    pub measures_rssi: bool,
}

/// The spacing used to place channels inside a sub-band on a continuously
/// tunable radio.
const CONTINUOUS_STEP_MHZ: f64 = 0.2;

impl ModuleCaps {
    /// Ebyte E22-900T22 (SX1262): 850.125 + CH x 1 MHz, CH 0..=80, 22/17/13/10 dBm.
    pub fn e22_900() -> Self {
        Self {
            band: "850-930".into(),
            tuning: Tuning::Grid {
                start_mhz: 850.125,
                step_mhz: 1.0,
                count: 81,
            },
            power_steps_dbm: vec![22, 17, 13, 10],
            measures_rssi: true,
        }
    }

    /// Ebyte E22-400T22 (SX1268): 410.125 + CH x 1 MHz, CH 0..=83.
    pub fn e22_400() -> Self {
        Self {
            band: "410-493".into(),
            tuning: Tuning::Grid {
                start_mhz: 410.125,
                step_mhz: 1.0,
                count: 84,
            },
            power_steps_dbm: vec![22, 17, 13, 10],
            measures_rssi: true,
        }
    }

    /// A Meshtastic node on an SX1262 covering 863..928 MHz: one preset slot,
    /// 1 dB power steps up to 22 dBm. The node does its own channel-activity
    /// detection; the host cannot sample its channel.
    pub fn meshtastic_863_928() -> Self {
        Self {
            band: "863-928".into(),
            tuning: Tuning::Continuous {
                lo_mhz: 863.0,
                hi_mhz: 928.0,
                hops: false,
            },
            power_steps_dbm: (1..=22).rev().collect(),
            measures_rssi: false,
        }
    }

    /// The loopback transport's unconstrained radio.
    pub fn any() -> Self {
        Self {
            band: "any".into(),
            tuning: Tuning::Continuous {
                lo_mhz: 0.0,
                hi_mhz: 100_000.0,
                hops: true,
            },
            power_steps_dbm: (0..=30).rev().collect(),
            measures_rssi: true,
        }
    }

    /// The caps for a `module_band` label (`850-930`, `410-493`, `863-928`,
    /// `any`).
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "850-930" => Some(Self::e22_900()),
            "410-493" => Some(Self::e22_400()),
            "863-928" => Some(Self::meshtastic_863_928()),
            "any" => Some(Self::any()),
            _ => None,
        }
    }

    /// Whether the module can tune a carrier centred on `mhz`.
    pub fn can_tune(&self, mhz: f64) -> bool {
        match self.tuning {
            Tuning::Grid {
                start_mhz,
                step_mhz,
                count,
            } => {
                let n = (mhz - start_mhz) / step_mhz;
                n >= -1e-6 && (n - n.round()).abs() < 1e-6 && n.round() < f64::from(count)
            }
            Tuning::Continuous { lo_mhz, hi_mhz, .. } => mhz >= lo_mhz && mhz <= hi_mhz,
        }
    }

    /// Whether the transport can change channel frame by frame.
    pub fn hops(&self) -> bool {
        match self.tuning {
            Tuning::Grid { .. } => true,
            Tuning::Continuous { hops, .. } => hops,
        }
    }
}

/// Whether `mhz` is a usable channel of `plan` on this module at `bw_khz`.
pub fn channel_ok(plan: &Plan, caps: &ModuleCaps, mhz: f64, bw_khz: u32) -> bool {
    plan.sub_band_for(mhz, bw_khz).is_some() && plan.on_grid(mhz) && caps.can_tune(mhz)
}

/// Every usable channel centre of `plan` on this module, ascending.
pub fn candidates(plan: &Plan, caps: &ModuleCaps, bw_khz: u32) -> Vec<f64> {
    let mut out: Vec<f64> = match (&caps.tuning, plan.grid) {
        (
            Tuning::Grid {
                start_mhz,
                step_mhz,
                count,
            },
            _,
        ) => (0..*count)
            .map(|n| start_mhz + f64::from(n) * step_mhz)
            .collect(),
        (Tuning::Continuous { .. }, Some(g)) => {
            let hi = plan.band_mhz.1;
            (0..)
                .map(|n| g.start_mhz + f64::from(n) * g.step_mhz)
                .take_while(|f| *f <= hi + 1e-9)
                .collect()
        }
        (Tuning::Continuous { .. }, None) => {
            let half = f64::from(bw_khz) / 2000.0;
            let mut v: Vec<f64> = plan
                .sub_bands
                .iter()
                .flat_map(|s| {
                    (0..)
                        .map(move |n| s.lo_mhz + half + f64::from(n) * CONTINUOUS_STEP_MHZ)
                        .take_while(move |f| f + half <= s.hi_mhz + 1e-9)
                })
                .collect();
            v.extend(plan.default_mhz);
            v
        }
    };
    out.retain(|f| channel_ok(plan, caps, *f, bw_khz));
    out.sort_by(f64::total_cmp);
    out.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    out
}

/// Whether the module meets `plan`: a usable channel exists and the module
/// offers the plan's minimum hop-channel count.
pub fn supported(plan: &Plan, caps: &ModuleCaps, bw_khz: u32) -> bool {
    if plan.lbt.is_some() && !caps.measures_rssi {
        return false;
    }
    let n = candidates(plan, caps, bw_khz).len() as u32;
    if n == 0 {
        return false;
    }
    let offered = if caps.hops() { n } else { 1 };
    plan.hop_channels_min == 0 || offered >= plan.hop_channels_min
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regions::by_code;

    #[test]
    fn e22_900_meets_the_india_plan_on_two_channels() {
        let c = candidates(by_code("IN").unwrap(), &ModuleCaps::e22_900(), 125);
        assert_eq!(c, vec![865.125, 866.125]);
        assert!(supported(
            by_code("IN").unwrap(),
            &ModuleCaps::e22_900(),
            125
        ));
    }

    #[test]
    fn e22_900_cannot_tune_cn470() {
        assert!(!supported(
            by_code("CN").unwrap(),
            &ModuleCaps::e22_900(),
            125
        ));
        assert!(supported(
            by_code("CN").unwrap(),
            &ModuleCaps::e22_400(),
            125
        ));
    }

    #[test]
    fn e22_900_is_off_the_japanese_and_korean_grids() {
        assert!(!supported(
            by_code("JP").unwrap(),
            &ModuleCaps::e22_900(),
            125
        ));
        assert!(!supported(
            by_code("KR").unwrap(),
            &ModuleCaps::e22_900(),
            125
        ));
        assert!(supported(by_code("JP").unwrap(), &ModuleCaps::any(), 125));
    }

    #[test]
    fn japan_on_a_meshtastic_node_is_a_mismatch_without_an_rssi_listen() {
        let jp = by_code("JP").unwrap();
        let mesh = ModuleCaps::meshtastic_863_928();
        // The grid is reachable (922.0 MHz), but listen-before-talk is not.
        assert!(!candidates(jp, &mesh, 125).is_empty());
        assert!(!supported(jp, &mesh, 125));
        let measuring = ModuleCaps {
            measures_rssi: true,
            ..mesh
        };
        assert!(supported(jp, &measuring, 125));
    }

    #[test]
    fn taiwan_needs_ten_hop_channels() {
        let tw = by_code("TW").unwrap();
        assert_eq!(candidates(tw, &ModuleCaps::e22_900(), 125).len(), 5);
        assert!(!supported(tw, &ModuleCaps::e22_900(), 125));
        // A Meshtastic node does not hop.
        assert!(!supported(tw, &ModuleCaps::meshtastic_863_928(), 125));
        assert!(supported(tw, &ModuleCaps::any(), 125));
    }

    #[test]
    fn us_hops_over_twenty_six_e22_channels() {
        let c = candidates(by_code("US").unwrap(), &ModuleCaps::e22_900(), 125);
        assert_eq!(c.len(), 26);
        assert_eq!(c[0], 902.125);
        assert_eq!(c[25], 927.125);
    }

    #[test]
    fn eu_p_sub_band_has_no_e22_channel() {
        let eu = by_code("EU").unwrap();
        let caps = ModuleCaps::e22_900();
        assert!(!channel_ok(eu, &caps, 869.525, 125));
        assert!(channel_ok(eu, &ModuleCaps::any(), 869.525, 125));
        assert!(channel_ok(eu, &caps, 868.125, 125));
    }
}
