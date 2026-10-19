//! `max_power`: the plan limit converted to conducted power at the module
//! port, rounded down to the module's steps, and adaptive power control.
//!
//! Conversion (`L` limit in dBm, `G` antenna gain in dBi, loss in dB):
//! ERP `L - G + 2.15 + loss`; EIRP `L - G + loss`; conducted `L`, reduced dB
//! for dB by the antenna gain above the rule's `antenna_max_dbi`. A PSD rule
//! caps the same way at `psd + 10 log10(BW_kHz / 3)`.

use std::collections::HashMap;

use crate::regions::{PowerRef, PowerRule};

/// ERP and EIRP differ by the gain of a half-wave dipole.
pub const DIPOLE_DBI: f64 = 2.15;

pub fn mw_to_dbm(mw: f64) -> f64 {
    10.0 * mw.log10()
}

pub fn dbm_to_mw(dbm: f64) -> f64 {
    10f64.powf(dbm / 10.0)
}

/// The antenna path the conversions use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Antenna {
    pub gain_dbi: f64,
    pub cable_loss_db: f64,
}

impl Antenna {
    /// Conducted dBm that radiates `ref_dbm` in `reference` terms.
    pub fn conducted_for(
        &self,
        ref_dbm: f64,
        reference: PowerRef,
        antenna_max_dbi: Option<f64>,
    ) -> f64 {
        match reference {
            PowerRef::Erp => ref_dbm - self.gain_dbi + DIPOLE_DBI + self.cable_loss_db,
            PowerRef::Eirp => ref_dbm - self.gain_dbi + self.cable_loss_db,
            PowerRef::Conducted => {
                ref_dbm - antenna_max_dbi.map_or(0.0, |max| (self.gain_dbi - max).max(0.0))
            }
        }
    }

    /// ERP in dBm radiated by `conducted_dbm` at the port.
    pub fn erp_of(&self, conducted_dbm: f64) -> f64 {
        conducted_dbm + self.gain_dbi - DIPOLE_DBI - self.cable_loss_db
    }
}

/// The highest conducted dBm the rule allows at `bw_khz`.
pub fn conducted_cap_dbm(rule: &PowerRule, ant: &Antenna, bw_khz: u32) -> f64 {
    let limit = ant.conducted_for(
        mw_to_dbm(rule.limit_mw),
        rule.reference,
        rule.antenna_max_dbi,
    );
    match rule.psd_dbm_per_3khz {
        Some(psd) => {
            let total = psd + 10.0 * (f64::from(bw_khz) / 3.0).log10();
            limit.min(ant.conducted_for(total, rule.reference, rule.antenna_max_dbi))
        }
        None => limit,
    }
}

/// The largest step not above `cap_dbm`, after skipping `steps_down` steps
/// below it (adaptive power control). `None` when even the lowest step is
/// over the cap.
pub fn pick_step(steps_desc: &[i8], cap_dbm: f64, steps_down: usize) -> Option<i8> {
    let fitting: Vec<i8> = steps_desc
        .iter()
        .copied()
        .filter(|s| f64::from(*s) <= cap_dbm + 1e-9)
        .collect();
    fitting.get(steps_down).or(fitting.last()).copied()
}

/// Adaptive power control: start at the clamp, step down while every peer
/// hears us with margin, step back up when any peer is marginal or silent.
#[derive(Debug, Default, Clone)]
pub struct Apc {
    pub steps_down: usize,
    /// Peer node -> (SNR of our last beacon it reported, when).
    peers: HashMap<u16, (f32, i64)>,
}

impl Apc {
    pub fn note_report(&mut self, peer: u16, snr_db: f32, at_ns: i64) {
        self.peers.insert(peer, (snr_db, at_ns));
    }

    /// Re-evaluate once per beacon interval. `floor_db` is the demodulation
    /// SNR floor of the spreading factor in use; `max_steps` how many steps
    /// below the clamp exist.
    pub fn update(&mut self, now_ns: i64, interval_ns: i64, floor_db: f64, max_steps: usize) {
        let stale = |at: i64| now_ns - at > 3 * interval_ns;
        let any_weak = self.peers.is_empty()
            || self
                .peers
                .values()
                .any(|(snr, at)| stale(*at) || f64::from(*snr) < floor_db + 4.0);
        let all_strong = !self.peers.is_empty()
            && self
                .peers
                .values()
                .all(|(snr, at)| !stale(*at) && f64::from(*snr) >= floor_db + 10.0);
        if any_weak {
            self.steps_down = self.steps_down.saturating_sub(1);
        } else if all_strong && self.steps_down < max_steps {
            self.steps_down += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E22: [i8; 4] = [22, 17, 13, 10];

    fn rule(limit_mw: f64, reference: PowerRef) -> PowerRule {
        PowerRule {
            limit_mw,
            reference,
            psd_dbm_per_3khz: None,
            antenna_max_dbi: None,
        }
    }

    #[test]
    fn india_clamp_with_a_5_dbi_antenna_needs_the_10_dbm_step() {
        let ant = Antenna {
            gain_dbi: 5.0,
            cable_loss_db: 0.0,
        };
        let cap = conducted_cap_dbm(&rule(25.0, PowerRef::Erp), &ant, 125);
        assert!((cap - 11.129).abs() < 0.01, "{cap}");
        assert_eq!(pick_step(&E22, cap, 0), Some(10));
    }

    #[test]
    fn india_clamp_with_a_dipole_runs_at_13_dbm() {
        let ant = Antenna {
            gain_dbi: 2.15,
            cable_loss_db: 0.0,
        };
        let cap = conducted_cap_dbm(&rule(25.0, PowerRef::Erp), &ant, 125);
        assert_eq!(pick_step(&E22, cap, 0), Some(13));
    }

    #[test]
    fn no_step_fits_is_none() {
        let ant = Antenna {
            gain_dbi: 9.0,
            cable_loss_db: 0.0,
        };
        // EU433 10 mW ERP with a 9 dBi antenna: 10 - 9 + 2.15 = 3.15 dBm.
        let cap = conducted_cap_dbm(&rule(10.0, PowerRef::Erp), &ant, 125);
        assert_eq!(pick_step(&E22, cap, 0), None);
    }

    #[test]
    fn us_psd_caps_conducted_power_at_24_2_dbm() {
        let r = PowerRule {
            limit_mw: 1000.0,
            reference: PowerRef::Conducted,
            psd_dbm_per_3khz: Some(8.0),
            antenna_max_dbi: Some(6.0),
        };
        let ant = Antenna {
            gain_dbi: 2.15,
            cable_loss_db: 0.0,
        };
        let cap = conducted_cap_dbm(&r, &ant, 125);
        assert!((cap - 24.198).abs() < 0.01, "{cap}");
        // An 8 dBi antenna takes 2 dB off the conducted limit too.
        let hi = Antenna {
            gain_dbi: 8.0,
            cable_loss_db: 0.0,
        };
        assert!((conducted_cap_dbm(&r, &hi, 125) - 22.198).abs() < 0.01);
    }

    #[test]
    fn apc_steps_down_with_margin_and_up_when_a_peer_goes_quiet() {
        let mut apc = Apc::default();
        let s = 1_000_000_000;
        apc.note_report(42, 0.0, 0);
        apc.update(s, 30 * s, -12.5, 3);
        assert_eq!(apc.steps_down, 1);
        apc.update(2 * s, 30 * s, -12.5, 3);
        assert_eq!(apc.steps_down, 2);
        // Silent for more than three intervals.
        apc.update(100 * s, 30 * s, -12.5, 3);
        assert_eq!(apc.steps_down, 1);
        assert_eq!(pick_step(&E22, 13.98, apc.steps_down), Some(10));
    }
}
