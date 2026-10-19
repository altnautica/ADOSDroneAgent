//! Validation of a `PUT /api/v1/lora` change against the plan it would run
//! under. A value outside the plan answers `E_LORA_OUT_OF_PLAN`.

use crate::airtime::{time_on_air_us, Modem};
use crate::band::{self, ModuleCaps};
use crate::governor::power::{conducted_cap_dbm, dbm_to_mw, Antenna};
use crate::regions::Plan;

/// The beacon frame size the dwell check is held to at `PUT` time.
pub const BEACON_FRAME_LEN: usize = 23;

/// The values a `PUT` may change, merged over the current config.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposed {
    pub freq_mhz: Option<f64>,
    pub erp_mw: Option<f64>,
    pub modem: Modem,
    pub transport: String,
    pub antenna: Antenna,
}

/// The ERP ceiling of the sub-band `mhz` sits in, in mW.
pub fn max_erp_mw(plan: &Plan, mhz: f64, bw_khz: u32, ant: &Antenna) -> Option<f64> {
    let rule = plan.sub_band_for(mhz, bw_khz)?.power?;
    Some(dbm_to_mw(ant.erp_of(conducted_cap_dbm(&rule, ant, bw_khz))))
}

/// `Ok` when `p` fits `plan` on the module `caps` (any module when unknown).
pub fn check(plan: &Plan, caps: Option<&ModuleCaps>, p: &Proposed) -> Result<(), String> {
    let any = ModuleCaps::any();
    let caps = caps.unwrap_or(&any);
    let bw = p.modem.bw_khz;
    if !(7..=12).contains(&p.modem.sf) {
        return Err(format!("sf {} is outside 7..=12", p.modem.sf));
    }
    if p.transport == "e22" && p.modem != Modem::SF9_BW125 {
        return Err("the e22 transport runs only the 2.4k air rate (SF9, 125 kHz, 4/5)".into());
    }
    if !plan.bandwidth_ok(bw) {
        return Err(format!(
            "{bw} kHz is wider than plan {} allows",
            plan.plan_id
        ));
    }
    let channel = match p.freq_mhz {
        Some(f) => {
            if !band::channel_ok(plan, caps, f, bw) {
                return Err(format!(
                    "{f} MHz is not a usable channel of plan {} on a {} module",
                    plan.plan_id, caps.band
                ));
            }
            Some(f)
        }
        None => plan
            .default_mhz
            .or_else(|| band::candidates(plan, caps, bw).first().copied()),
    };
    if let (Some(erp), Some(f)) = (p.erp_mw, channel) {
        if erp <= 0.0 {
            return Err("erp_mw must be positive".into());
        }
        if let Some(max) = max_erp_mw(plan, f, bw, &p.antenna) {
            if erp > max + 1e-6 {
                return Err(format!(
                    "erp_mw {erp} is above the {max:.2} mW plan {} allows at {f} MHz",
                    plan.plan_id
                ));
            }
        }
    }
    if let Some(dwell) = plan.dwell {
        let factor = if p.transport == "e22" { 2 } else { 1 };
        let toa = time_on_air_us(&p.modem, BEACON_FRAME_LEN) * factor;
        if toa > u64::from(dwell.single_tx_max_ms) * 1000 {
            return Err(format!(
                "a beacon is {} ms on air, over the {} ms single transmission plan {} allows",
                toa / 1000,
                dwell.single_tx_max_ms,
                plan.plan_id
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regions::by_code;

    fn proposed(freq: Option<f64>, erp: Option<f64>) -> Proposed {
        Proposed {
            freq_mhz: freq,
            erp_mw: erp,
            modem: Modem::SF9_BW125,
            transport: "e22".into(),
            antenna: Antenna {
                gain_dbi: 2.15,
                cable_loss_db: 0.0,
            },
        }
    }

    #[test]
    fn india_rejects_an_erp_over_the_clamp_and_an_off_plan_frequency() {
        let p = by_code("IN").unwrap();
        let e22 = ModuleCaps::e22_900();
        assert!(check(p, Some(&e22), &proposed(Some(866.125), Some(25.0))).is_ok());
        assert!(check(p, Some(&e22), &proposed(None, Some(26.0))).is_err());
        assert!(check(p, Some(&e22), &proposed(Some(868.125), None)).is_err());
        // 866.0 is in the plan but not on the E22 grid.
        assert!(check(p, Some(&e22), &proposed(Some(866.0), None)).is_err());
    }

    #[test]
    fn e22_beacons_cannot_meet_the_us_single_transmission_limit() {
        let us = by_code("US").unwrap();
        assert!(check(us, Some(&ModuleCaps::e22_900()), &proposed(None, None)).is_err());
        let mut lb = proposed(None, None);
        lb.transport = "loopback".into();
        assert!(check(us, None, &lb).is_ok());
    }
}
