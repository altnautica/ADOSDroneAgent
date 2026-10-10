//! TX-power application with the fallback ramp + the muted-readback policy.
//!
//! Applies `iw set txpower fixed`, ramping UP through driver-acceptable
//! fallbacks that stay inside the configured ceiling, and reads the value
//! back: a readback at the muted not-permitted floor means the PHY accepts
//! injected frames but radiates nothing, which the region posture treats as a
//! dead radio and the unrestricted posture surfaces as `rf_unverified` while
//! letting bring-up continue.

use super::{run_cmd, run_cmd_output};

/// Build the TX-power fallback ramp: the requested dBm first, then 5/7/10 dBm
/// (each only if it is higher than the request and not already present). A
/// low request can be rejected by the driver depending on the regulatory
/// domain, so we ramp UP to a value it accepts.
///
/// Every step is held at or below `max_dbm`, the configured power ceiling: a
/// fallback above the ceiling is dropped, and a request above it yields an
/// empty ramp. The ceiling guards a host-VBUS adapter against brownout and the
/// operator's regulatory limit, so a rejection is never answered by going past
/// it.
pub fn tx_power_ramp(dbm: i8, max_dbm: i8) -> Vec<i8> {
    if dbm > max_dbm {
        return Vec::new();
    }
    let mut ramp = vec![dbm];
    for fallback in [5i8, 7, 10] {
        if fallback > dbm && fallback <= max_dbm && !ramp.contains(&fallback) {
            ramp.push(fallback);
        }
    }
    ramp
}

/// The two `iw` operations the TX-power apply drives, behind a seam so the
/// ramp + readback policy is testable with a scripted runner.
#[async_trait::async_trait]
pub trait TxPowerIo: Send + Sync {
    /// `iw dev <iface> set txpower fixed <mbm>`; `true` on a zero exit.
    async fn set_fixed_mbm(&self, iface: &str, mbm: i32) -> bool;
    /// The live TX power readback in dBm, or `None` when unreadable.
    async fn read_dbm(&self, iface: &str) -> Option<f32>;
}

/// The production [`TxPowerIo`]: real `iw` subprocesses.
pub struct IwTxPower;

#[async_trait::async_trait]
impl TxPowerIo for IwTxPower {
    async fn set_fixed_mbm(&self, iface: &str, mbm: i32) -> bool {
        run_cmd(
            "iw",
            &["dev", iface, "set", "txpower", "fixed", &mbm.to_string()],
        )
        .await
        .is_ok()
    }

    async fn read_dbm(&self, iface: &str) -> Option<f32> {
        read_tx_power(iface).await
    }
}

/// Readback floor below which a `txpower` value means the interface is pinned at
/// the regulatory "not permitted" floor (reported as `-100.00 dBm`), i.e. a muted
/// PHY that accepts injected frames but radiates nothing. Any genuine setting is
/// >= 0 dBm, so a readback at or below this is unambiguously the muted floor.
pub const MUTED_TX_POWER_DBM: f32 = -10.0;

/// Parse the `txpower <dBm> dBm` line out of an `iw dev <iface> info` body.
/// Pure helper shared with the single-read heartbeat path.
pub fn parse_iface_txpower(info: &str) -> Option<f32> {
    for line in info.lines() {
        if let Some(rest) = line.trim().strip_prefix("txpower ") {
            return rest.split_whitespace().next()?.parse::<f32>().ok();
        }
    }
    None
}

/// Read the live TX power (dBm) from `iw dev <iface> info`, or `None` when it
/// cannot be read or parsed. Used to verify a `set txpower` actually took.
pub async fn read_tx_power(iface: &str) -> Option<f32> {
    let out = run_cmd_output("iw", &["dev", iface, "info"]).await.ok()?;
    parse_iface_txpower(&out)
}

/// Apply TX power via `iw dev <iface> set txpower fixed <mBm>` (mBm = dBm×100),
/// ramping up through the in-budget fallbacks on driver rejection, with the
/// operating posture threaded through. Returns the dBm the driver accepted, or
/// `None` when no step inside `max_dbm` was accepted: the caller parks and
/// retries, and the ceiling is never exceeded.
///
/// **Why this matters:** without it the dongle runs at the driver default
/// (~17-20 dBm), which browns out a host-VBUS-powered RTL adapter.
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
pub async fn set_tx_power_modal(
    iface: &str,
    dbm: i8,
    max_dbm: i8,
    unrestricted: bool,
) -> Option<i8> {
    apply_tx_power_with(&IwTxPower, iface, dbm, max_dbm, unrestricted).await
}

/// [`set_tx_power_modal`] over an explicit [`TxPowerIo`] (the test seam).
pub async fn apply_tx_power_with<I: TxPowerIo + ?Sized>(
    io: &I,
    iface: &str,
    dbm: i8,
    max_dbm: i8,
    unrestricted: bool,
) -> Option<i8> {
    for candidate in tx_power_ramp(dbm, max_dbm) {
        let mbm = (candidate as i32) * 100;
        if !io.set_fixed_mbm(iface, mbm).await {
            continue;
        }
        // A zero exit from `iw set txpower` is necessary but NOT sufficient: a
        // regulatory/PHY perturbation (e.g. an `iw reg set` churn re-entering
        // monitor) can leave the interface pinned at the "-100 dBm" not-permitted
        // floor while the set command still returns success. That muted PHY
        // injects frames into the TX ring (the counter advances) but radiates
        // nothing. Read the value back: in region mode this is a fatal "dead
        // radio" verdict; under the unrestricted posture we log + surface it
        // (rf_unverified) but let bring-up continue.
        if let Some(live) = io.read_dbm(iface).await {
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
                return Some(candidate);
            }
        }
        if candidate != dbm {
            tracing::warn!(
                iface,
                requested = dbm,
                applied = candidate,
                max_dbm,
                "wfb_txpower_fallback"
            );
        } else {
            tracing::info!(iface, dbm = candidate, "wfb_txpower_applied");
        }
        return Some(candidate);
    }
    tracing::error!(
        iface,
        requested = dbm,
        max_dbm,
        "wfb_txpower_no_in_budget_step_accepted"
    );
    None
}

/// A scripted [`TxPowerIo`] for tests across the crate: accepts a set at or
/// above `accept_from_mbm` (never, when `None`), reads back `readback`, and
/// records the highest mBm ever requested plus the number of set calls.
#[cfg(test)]
pub(crate) struct ScriptedTxPower {
    pub(crate) accept_from_mbm: Option<i32>,
    pub(crate) readback: Option<f32>,
    pub(crate) max_requested_mbm: std::sync::atomic::AtomicI32,
    pub(crate) set_calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl ScriptedTxPower {
    pub(crate) fn new(accept_from_mbm: Option<i32>, readback: Option<f32>) -> Self {
        Self {
            accept_from_mbm,
            readback,
            max_requested_mbm: std::sync::atomic::AtomicI32::new(i32::MIN),
            set_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn max_requested(&self) -> i32 {
        self.max_requested_mbm
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl TxPowerIo for ScriptedTxPower {
    async fn set_fixed_mbm(&self, _iface: &str, mbm: i32) -> bool {
        use std::sync::atomic::Ordering;
        self.set_calls.fetch_add(1, Ordering::Relaxed);
        self.max_requested_mbm.fetch_max(mbm, Ordering::Relaxed);
        self.accept_from_mbm.is_some_and(|from| mbm >= from)
    }

    async fn read_dbm(&self, _iface: &str) -> Option<f32> {
        self.readback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_power_ramp_low_request_under_low_ceiling_has_no_fallbacks() {
        // A 1 dBm request under a 3 dBm ceiling: every fallback is above it.
        assert_eq!(tx_power_ramp(1, 3), vec![1]);
    }

    #[test]
    fn tx_power_ramp_low_request_adds_in_budget_fallbacks() {
        assert_eq!(tx_power_ramp(5, 15), vec![5, 7, 10]);
        assert_eq!(tx_power_ramp(5, 7), vec![5, 7]);
    }

    #[test]
    fn tx_power_ramp_high_request_has_no_fallbacks() {
        assert_eq!(tx_power_ramp(15, 15), vec![15]);
        assert_eq!(tx_power_ramp(7, 15), vec![7, 10]);
    }

    #[test]
    fn tx_power_ramp_request_above_ceiling_is_empty() {
        assert!(tx_power_ramp(12, 10).is_empty());
    }

    #[test]
    fn parse_iface_txpower_reads_the_dbm() {
        let info = "Interface wlan1\n\tchannel 149 (5745 MHz)\n\ttxpower 5.00 dBm\n";
        assert_eq!(parse_iface_txpower(info), Some(5.0));
        assert_eq!(parse_iface_txpower("Interface wlan1\n"), None);
    }

    #[tokio::test]
    async fn a_rejected_request_never_ramps_past_the_ceiling() {
        let io = ScriptedTxPower::new(None, Some(20.0));
        assert_eq!(apply_tx_power_with(&io, "wlan1", 1, 3, false).await, None);
        assert_eq!(io.max_requested(), 100);
    }

    #[tokio::test]
    async fn an_in_budget_fallback_is_accepted() {
        // The driver rejects 1 dBm but takes 5 dBm; the ceiling is 7.
        let io = ScriptedTxPower::new(Some(500), Some(5.0));
        assert_eq!(
            apply_tx_power_with(&io, "wlan1", 1, 7, false).await,
            Some(5)
        );
        assert_eq!(io.max_requested(), 500);
    }

    #[tokio::test]
    async fn a_muted_readback_aborts_in_region_and_surfaces_when_unrestricted() {
        let io = ScriptedTxPower::new(Some(0), Some(-100.0));
        assert_eq!(apply_tx_power_with(&io, "wlan1", 5, 10, false).await, None);
        assert_eq!(
            apply_tx_power_with(&io, "wlan1", 5, 10, true).await,
            Some(5)
        );
    }
}
