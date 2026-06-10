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

/// Apply TX power AND confirm the PHY is actually radiating — i.e. the txpower
/// readback is off the muted not-permitted floor. Under the unrestricted posture
/// `set_tx_power_modal` returns `Some` even on a muted readback (it surfaces the
/// honest rf_unverified signal and lets bring-up continue), so a bare call
/// cannot distinguish a radiating PHY from a muted one. wfb_tx injecting into a
/// muted PHY fails every `sendmsg` with ENOBUFS (tx_bytes frozen) and the
/// liveness watchdog then kills + respawns wfb_tx forever with no effect. Each
/// attempt re-runs the proven down -> monitor -> up -> channel cycle right before
/// the txpower set, then verifies the readback. Returns the effective dBm once
/// the readback is non-muted, or `None` if it stays muted after every attempt —
/// the caller must NOT start wfb_tx on a `None` (park and retry bring-up).
pub(crate) async fn ensure_radiating(
    iface: &str,
    channel: u8,
    dbm: i8,
    unrestricted: bool,
) -> Option<i8> {
    for attempt in 0..PHY_RADIATE_MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300 + 300 * attempt as u64)).await;
        }
        // The un-stick: a fresh interface cycle (down -> monitor -> up) + channel
        // right before the txpower set. set_monitor_mode_verified refuses the
        // operator's control interface, so this can never sever management.
        ados_radio::adapter::set_monitor_mode_verified(iface, 2).await;
        set_channel(iface, channel).await;
        let applied = ados_radio::adapter::set_tx_power_modal(iface, dbm, unrestricted).await;
        let live = ados_radio::adapter::read_tx_power(iface).await;
        if let Some(dbm_live) = live {
            if dbm_live > ados_radio::adapter::MUTED_TX_POWER_DBM {
                tracing::info!(
                    iface,
                    channel,
                    attempt,
                    readback_dbm = dbm_live,
                    "wfb_phy_radiating"
                );
                return applied.or(Some(dbm));
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

/// `iw <iface> set channel <ch>`, VERIFIED. Returns `true` only when the
/// command exits 0 AND a readback of `iw <iface> info` confirms the interface
/// landed on `channel`. A silent driver no-op (exit 0 but the channel never
/// changed) and a hung `iw` both record `false` instead of a false success, so
/// the caller's hop / return-home outcome reflects reality.
pub(crate) async fn set_channel(iface: &str, channel: u8) -> bool {
    let status = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "set", "channel", &channel.to_string()])
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
/// `iw` cannot be run or its output carries no channel. Split out so the
/// readback parse is unit-testable independently of the subprocess.
pub(crate) async fn channel_from_iface(iface: &str) -> Option<u8> {
    let out = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_iface_channel(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `channel <N>` token out of an `iw <iface> info` body. The line
/// reads e.g. `\tchannel 149 (5745 MHz), width: 20 MHz, …`; the first integer
/// after the `channel` keyword is the channel number. Pure helper.
pub(crate) fn parse_iface_channel(info: &str) -> Option<u8> {
    for line in info.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "channel" {
                if let Some(n) = toks.next() {
                    if let Ok(ch) = n.parse::<u8>() {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iface_channel_reads_channel_token() {
        // The readback seam set_channel uses to verify the live channel. The
        // verified-bool is `set_ok && parse == target`; here we exercise the
        // parse half so a silent driver no-op (info still shows the old channel)
        // is distinguishable from a real retune.
        let info = "Interface wlan1\n\tifindex 5\n\ttype monitor\n\
                    \tchannel 149 (5745 MHz), width: 20 MHz, center1: 5745 MHz\n";
        assert_eq!(parse_iface_channel(info), Some(149));
        // A different live channel parses to its own value, so a mismatch
        // against the requested target records ok=false.
        let other = "Interface wlan1\n\tchannel 36 (5180 MHz), width: 20 MHz\n";
        assert_eq!(parse_iface_channel(other), Some(36));
    }

    #[test]
    fn parse_iface_channel_no_channel_is_none() {
        // No `channel` line (radio not on a channel, or unreadable info) → None,
        // which set_channel treats as an unverified failure (ok=false).
        assert_eq!(
            parse_iface_channel("Interface wlan1\n\ttype managed\n"),
            None
        );
        assert_eq!(parse_iface_channel(""), None);
        // A bare `channel` keyword with no number is also None, not a panic.
        assert_eq!(parse_iface_channel("\tchannel\n"), None);
    }
}
