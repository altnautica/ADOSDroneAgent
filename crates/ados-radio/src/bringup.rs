//! Interface bring-up helpers: land monitor mode + the channel, coax the PHY
//! off the muted floor, and the verified `iw set channel` + readback parse.
//!
//! These are the pre-injection guards the service runs before it starts
//! `wfb_tx`: starting injection on a managed / mis-tuned / muted interface
//! advances `tx_bytes` while radiating nothing a ground station can decode, so
//! the bring-up must verify reality (a channel readback, a non-muted txpower
//! readback) instead of trusting the command's exit code.

use std::time::Duration;

/// Per-call ceiling on the `iw set channel` + readback so a hung `iw` (driver
/// wedged mid-retune) cannot stall the hop / return-home path.
pub(crate) const SET_CHANNEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Bring-up attempts at landing monitor mode + the channel before the service
/// re-enters its selection loop. A self-managed injection PHY (the RTL family)
/// or a concurrent regulatory-domain set on the same wiphy can revert the vif
/// to managed between adapter selection and the channel set, which makes
/// `iw set channel` fail EBUSY; re-asserting monitor mode (down → monitor → up)
/// before each channel attempt reclaims a reverted vif.
const CHANNEL_SET_MAX_ATTEMPTS: u32 = 4;

/// Land the interface in monitor mode AND on `channel`, verified, with bounded
/// retries. Each attempt re-asserts monitor mode (the full down → monitor → up
/// sequence — idempotent when already monitor, but it reclaims a vif that
/// silently reverted to managed after selection) and then sets + verifies the
/// channel. Returns `true` only on a verified channel readback. The caller must
/// not start `wfb_tx` on a `false`, or it injects on a managed / mis-tuned
/// interface that advances `tx_bytes` while radiating nothing a ground station
/// can decode (an advancing TX counter with zero usable RF).
pub(crate) async fn ensure_monitor_and_channel(iface: &str, channel: u8) -> bool {
    for attempt in 0..CHANNEL_SET_MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
        }
        // Force monitor mode (corrects a vif that reverted to managed). The
        // helper refuses the operator's control interface, so this can never
        // sever the management link.
        ados_radio::adapter::set_monitor_mode_verified(iface, 2).await;
        if set_channel(iface, channel).await {
            if attempt > 0 {
                tracing::info!(iface, channel, attempt, "wfb_channel_set_recovered");
            }
            return true;
        }
        tracing::warn!(
            iface,
            channel,
            attempt,
            "wfb_channel_set_retry: re-asserting monitor mode before retry"
        );
    }
    false
}

/// Attempts at coaxing the PHY off the muted "-100 dBm" not-permitted floor
/// during bring-up. A self-managed RTL PHY can wedge at the floor after the
/// monitor/channel/reg bring-up churn; a fresh down -> monitor -> up -> channel
/// cycle immediately before the txpower set un-sticks it (bench-proven: the
/// configured floor un-mutes once the interface cycle precedes the set).
const PHY_RADIATE_MAX_ATTEMPTS: u32 = 4;

/// Apply TX power AND confirm the PHY is actually radiating on `channel` — i.e.
/// the channel reads back as `channel` and the txpower readback is off the muted
/// not-permitted floor. Under the unrestricted posture
/// `set_tx_power_modal` returns `Some` even on a muted readback (it surfaces the
/// honest rf_unverified signal and lets bring-up continue), so a bare call
/// cannot distinguish a radiating PHY from a muted one. wfb_tx injecting into a
/// muted PHY fails every `sendmsg` with ENOBUFS (tx_bytes frozen) and the
/// liveness watchdog then kills + respawns wfb_tx forever with no effect. Each
/// attempt re-runs the proven down -> monitor -> up -> channel cycle right before
/// the txpower set, then verifies both readbacks: the interface cycle can drop
/// the channel, and a mis-tuned interface radiates nothing the peer can decode.
///
/// Every ramp step stays at or below `max_dbm`. An attempt where the driver
/// accepted no in-budget step is a failed attempt even when the readback is off
/// the floor: the PHY is then running at its driver default, not at a power this
/// service set. Returns the MEASURED dBm once both hold (the accepted step when
/// the readback agrees within 1 dB, the rounded readback otherwise), or `None`
/// after every attempt — the caller must NOT start (or keep) wfb_tx on a `None`.
pub(crate) async fn ensure_radiating(
    iface: &str,
    channel: u8,
    dbm: i8,
    max_dbm: i8,
    unrestricted: bool,
) -> Option<i8> {
    ensure_radiating_with(&LiveRadiate, iface, channel, dbm, max_dbm, unrestricted).await
}

/// The three steps [`ensure_radiating`] drives, behind a seam so the
/// accept/readback policy is testable without a radio.
#[async_trait::async_trait]
pub(crate) trait RadiateIo: Send + Sync {
    /// Re-cycle the interface into monitor mode and land `channel`, verified.
    async fn cycle_and_tune(&self, iface: &str, channel: u8) -> bool;
    /// Apply the bounded TX-power ramp; the accepted dBm or `None`.
    async fn apply(&self, iface: &str, dbm: i8, max_dbm: i8, unrestricted: bool) -> Option<i8>;
    /// The live TX power readback.
    async fn read(&self, iface: &str) -> Option<f32>;
}

/// The production [`RadiateIo`]: real `iw` calls.
pub(crate) struct LiveRadiate;

#[async_trait::async_trait]
impl RadiateIo for LiveRadiate {
    async fn cycle_and_tune(&self, iface: &str, channel: u8) -> bool {
        // The un-stick: a fresh interface cycle (down -> monitor -> up) +
        // channel right before the txpower set. set_monitor_mode_verified
        // refuses the operator's control interface, so this can never sever
        // management.
        ados_radio::adapter::set_monitor_mode_verified(iface, 2).await;
        set_channel(iface, channel).await
    }

    async fn apply(&self, iface: &str, dbm: i8, max_dbm: i8, unrestricted: bool) -> Option<i8> {
        ados_radio::adapter::set_tx_power_modal(iface, dbm, max_dbm, unrestricted).await
    }

    async fn read(&self, iface: &str) -> Option<f32> {
        ados_radio::adapter::read_tx_power(iface).await
    }
}

/// Largest gap between the accepted step and the readback that still counts as
/// the driver honouring the set.
const READBACK_TOLERANCE_DB: f32 = 1.0;

/// [`ensure_radiating`] over an explicit [`RadiateIo`].
pub(crate) async fn ensure_radiating_with<I: RadiateIo + ?Sized>(
    io: &I,
    iface: &str,
    channel: u8,
    dbm: i8,
    max_dbm: i8,
    unrestricted: bool,
) -> Option<i8> {
    for attempt in 0..PHY_RADIATE_MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300 + 300 * attempt as u64)).await;
        }
        if !io.cycle_and_tune(iface, channel).await {
            tracing::warn!(
                iface,
                channel,
                attempt,
                "wfb_phy_channel_unverified_retry: re-cycling interface"
            );
            continue;
        }
        let Some(applied) = io.apply(iface, dbm, max_dbm, unrestricted).await else {
            tracing::warn!(
                iface,
                requested = dbm,
                max_dbm,
                attempt,
                "wfb_txpower_no_step_accepted_retry"
            );
            continue;
        };
        let live = io.read(iface).await;
        if let Some(dbm_live) = live {
            if dbm_live > ados_radio::adapter::MUTED_TX_POWER_DBM {
                tracing::info!(
                    iface,
                    channel,
                    attempt,
                    readback_dbm = dbm_live,
                    "wfb_phy_radiating"
                );
                return Some(measured_dbm(iface, applied, dbm_live));
            }
        }
        tracing::warn!(
            iface,
            channel,
            attempt,
            readback_dbm = ?live,
            "wfb_phy_muted_retry: re-cycling interface before re-setting txpower"
        );
    }
    None
}

/// The power to report for an accepted step: the step itself when the live
/// readback agrees within [`READBACK_TOLERANCE_DB`], else the rounded readback
/// (what the PHY actually runs), with a warning.
fn measured_dbm(iface: &str, applied: i8, live: f32) -> i8 {
    if (live - applied as f32).abs() <= READBACK_TOLERANCE_DB {
        return applied;
    }
    tracing::warn!(
        iface,
        applied,
        readback_dbm = live,
        "tx_power_readback_mismatch"
    );
    live.round().clamp(i8::MIN as f32, i8::MAX as f32) as i8
}

/// `iw <iface> set channel <ch>`, VERIFIED. Returns `true` only when the
/// command exits 0 AND a readback of `iw <iface> info` confirms the interface
/// landed on `channel`. A silent driver no-op (exit 0 but the channel never
/// changed) and a hung `iw` both record `false` instead of a false success, so
/// the caller's hop / return-home outcome reflects reality.
pub(crate) async fn set_channel(iface: &str, channel: u8) -> bool {
    // `kill_on_drop` is what makes the timeout a real bound: dropping the
    // future at the deadline otherwise leaves the wedged `iw` running, one per
    // timed-out hop, on the board that is already in trouble.
    let status = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "set", "channel", &channel.to_string()])
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match status {
        Ok(Ok(s)) if s.success() => {}
        Ok(Ok(s)) => {
            tracing::warn!(iface, channel, exit = s.code(), "iw_set_channel_failed");
            return false;
        }
        Ok(Err(e)) => {
            tracing::warn!(iface, channel, error = %e, "iw_set_channel_error");
            return false;
        }
        Err(_) => {
            tracing::warn!(iface, channel, "iw_set_channel_timeout");
            return false;
        }
    }
    // Read back the live channel; a mismatch (or unreadable info) is a failure.
    match channel_from_iface(iface).await {
        Some(live) if live == channel => true,
        Some(live) => {
            tracing::warn!(iface, channel, live, "iw_set_channel_readback_mismatch");
            false
        }
        None => {
            tracing::warn!(iface, channel, "iw_set_channel_readback_unavailable");
            false
        }
    }
}

/// Read the interface's current channel from `iw <iface> info`, or `None` when
/// `iw` cannot be run or its output carries no channel.
pub(crate) async fn channel_from_iface(iface: &str) -> Option<u8> {
    let out = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    ados_radio::adapter::parse_iface_channel(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tunes fine; the TX-power apply runs the real bounded ramp over a
    /// scripted driver; the readback is fixed.
    struct FakeRadiate {
        accept_from_mbm: Option<i32>,
        readback: Option<f32>,
    }

    #[async_trait::async_trait]
    impl RadiateIo for FakeRadiate {
        async fn cycle_and_tune(&self, _iface: &str, _channel: u8) -> bool {
            true
        }
        async fn apply(&self, iface: &str, dbm: i8, max_dbm: i8, unrestricted: bool) -> Option<i8> {
            let io = ScriptedSet {
                accept_from_mbm: self.accept_from_mbm,
                readback: self.readback,
            };
            ados_radio::adapter::apply_tx_power_with(&io, iface, dbm, max_dbm, unrestricted).await
        }
        async fn read(&self, _iface: &str) -> Option<f32> {
            self.readback
        }
    }

    struct ScriptedSet {
        accept_from_mbm: Option<i32>,
        readback: Option<f32>,
    }

    #[async_trait::async_trait]
    impl ados_radio::adapter::TxPowerIo for ScriptedSet {
        async fn set_fixed_mbm(&self, _iface: &str, mbm: i32) -> bool {
            self.accept_from_mbm.is_some_and(|from| mbm >= from)
        }
        async fn read_dbm(&self, _iface: &str) -> Option<f32> {
            self.readback
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_radiating_rejects_all_steps_returns_none() {
        // Every step is rejected while the PHY reads back 20 dBm (the driver
        // default): that is not a power this service set, so no value.
        let io = FakeRadiate {
            accept_from_mbm: None,
            readback: Some(20.0),
        };
        assert_eq!(
            ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn readback_mismatch_reports_live() {
        // 5 dBm accepted, but the PHY reads back 10 dBm: report what it runs.
        let io = FakeRadiate {
            accept_from_mbm: Some(0),
            readback: Some(10.0),
        };
        assert_eq!(
            ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
            Some(10)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn readback_within_tolerance_reports_the_step() {
        let io = FakeRadiate {
            accept_from_mbm: Some(0),
            readback: Some(5.5),
        };
        assert_eq!(
            ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
            Some(5)
        );
    }
}
