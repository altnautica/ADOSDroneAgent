//! TX-power application with the fallback ramp + the muted-readback policy.
//!
//! Applies `iw set txpower fixed`, ramping UP through driver-acceptable
//! fallbacks, and reads the value back: a readback at the muted not-permitted
//! floor means the PHY accepts injected frames but radiates nothing, which the
//! region posture treats as a dead radio and the unrestricted posture surfaces
//! as `rf_unverified` while letting bring-up continue.

use super::{run_cmd, run_cmd_output};

/// Build the TX-power fallback ramp: the requested dBm first, then 5/7/10 dBm
/// (each only if it is higher than the request and not already present).
/// Mirrors `adapter.py:689-692`. A low request can be rejected by the driver
/// depending on the regulatory domain, so we ramp UP to a value it accepts.
pub fn tx_power_ramp(dbm: i8) -> Vec<i8> {
    let mut ramp = vec![dbm];
    for fallback in [5i8, 7, 10] {
        if fallback > dbm && !ramp.contains(&fallback) {
            ramp.push(fallback);
        }
    }
    ramp
}

/// Apply TX power via `iw dev <iface> set txpower fixed <mBm>` (mBm = dBm×100),
/// ramping up through the fallbacks on driver rejection. Returns the effective
/// dBm that was accepted, or `None` if every step failed.
///
/// **Why this matters:** without it the dongle runs at the driver default
/// (~17-20 dBm), which browns out a host-VBUS-powered RTL adapter — the exact
/// failure `video.wfb.tx_power_dbm = 5` guards against (`adapter.py:674-732`).
/// Readback floor below which a `txpower` value means the interface is pinned at
/// the regulatory "not permitted" floor (reported as `-100.00 dBm`), i.e. a muted
/// PHY that accepts injected frames but radiates nothing. Any genuine setting is
/// >= 0 dBm, so a readback at or below this is unambiguously the muted floor.
pub const MUTED_TX_POWER_DBM: f32 = -10.0;

/// Read the live TX power (dBm) from `iw dev <iface> info`, or `None` when it
/// cannot be read or parsed. Used to verify a `set txpower` actually took.
pub async fn read_tx_power(iface: &str) -> Option<f32> {
    let out = run_cmd_output("iw", &["dev", iface, "info"]).await.ok()?;
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("txpower ") {
            return rest.split_whitespace().next()?.parse::<f32>().ok();
        }
    }
    None
}

/// Apply TX power with the region-mode muted-readback policy (region: abort on a
/// muted PHY by returning None). Thin wrapper over [`set_tx_power_modal`] so
/// existing region-mode callers stay unchanged.
pub async fn set_tx_power(iface: &str, dbm: i8) -> Option<i8> {
    set_tx_power_modal(iface, dbm, false).await
}

/// Apply TX power, threading the unrestricted operating posture.
///
/// `unrestricted` controls only the muted-readback handling. A muted readback
/// (<= the not-permitted floor) is ALWAYS logged + surfaces as `rf_unverified`
/// downstream (the received-side dual-check stays armed in both postures). The
/// difference is the return value:
/// - region mode (`unrestricted == false`): return `None` — the muted PHY is a
///   dead radio under an enforced region, so the caller does not promote it.
/// - unrestricted mode (`unrestricted == true`): return `Some(candidate)` — under
///   the operator-responsible posture we surface the muted state rather than
///   aborting bring-up, because a self-managed PHY can read back muted on the
///   not-permitted floor before the driver-region layer settles, yet still need
///   the rest of the bring-up to proceed. The honest `rf_unverified` signal is
///   what tells the operator the radio is not yet radiating.
pub async fn set_tx_power_modal(iface: &str, dbm: i8, unrestricted: bool) -> Option<i8> {
    for candidate in tx_power_ramp(dbm) {
        let mbm = (candidate as i32) * 100;
        if run_cmd(
            "iw",
            &["dev", iface, "set", "txpower", "fixed", &mbm.to_string()],
        )
        .await
        .is_ok()
        {
            // A zero exit from `iw set txpower` is necessary but NOT sufficient:
            // a regulatory/PHY perturbation (e.g. an `iw reg set` churn re-entering
            // monitor) can leave the interface pinned at the "-100 dBm" not-permitted
            // floor while the set command still returns success. That muted PHY
            // injects frames into the TX ring (the counter advances) but radiates
            // nothing. Read the value back: in region mode this is a fatal "dead
            // radio" verdict; under the unrestricted posture we log + surface it
            // (rf_unverified) but let bring-up continue.
            if let Some(live) = read_tx_power(iface).await {
                if live <= MUTED_TX_POWER_DBM {
                    tracing::error!(
                        iface,
                        requested = dbm,
                        applied = candidate,
                        readback_dbm = live,
                        unrestricted,
                        "wfb_txpower_muted_readback"
                    );
                    if !unrestricted {
                        return None;
                    }
                    // Unrestricted: surface, do not abort. The muted readback is
                    // still honestly reported; return the candidate so the rest of
                    // bring-up proceeds and the rf_unverified detector carries the
                    // truth that no RF is leaving the antenna.
                    return Some(candidate);
                }
            }
            if candidate != dbm {
                tracing::warn!(
                    iface,
                    requested = dbm,
                    applied = candidate,
                    "wfb_txpower_fallback"
                );
            } else {
                tracing::info!(iface, dbm = candidate, "wfb_txpower_applied");
            }
            return Some(candidate);
        }
    }
    tracing::error!(iface, requested = dbm, "wfb_txpower_all_steps_rejected");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_power_ramp_low_request_adds_fallbacks() {
        // A 5 dBm request: 5 is not < itself, 7 and 10 are higher → ramp up.
        assert_eq!(tx_power_ramp(5), vec![5, 7, 10]);
    }

    #[test]
    fn tx_power_ramp_high_request_has_no_fallbacks() {
        // A 15 dBm request: no fallback exceeds it.
        assert_eq!(tx_power_ramp(15), vec![15]);
    }

    #[test]
    fn tx_power_ramp_mid_request() {
        // 7 dBm: only 10 is higher.
        assert_eq!(tx_power_ramp(7), vec![7, 10]);
    }
}
