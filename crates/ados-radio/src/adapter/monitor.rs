//! Monitor-mode set / verify / restore for the injection interface.
//!
//! Sets the RTL adapter into monitor mode (with the load-bearing
//! `monitor none` form first), verifies the readback, and re-asserts monitor +
//! retunes when a scan strands the iface in managed mode. The control-interface
//! guard (in the barrel) is consulted before any down/type change so the
//! operator's management link can never be flipped to monitor.

use super::{control_interface, run_cmd, run_cmd_output};

/// Set monitor mode on an interface and verify the readback. Retries up to
/// `max_retries` times with 500ms backoff. Returns true only on verified
/// monitor mode.
///
/// Sequence (mirrors adapter.py:471-561):
///   nmcli dev set <if> managed no  (best-effort)
///   ip link set <if> down
///   iw <if> set type monitor        (primary)
///     fallback: iw <if> set monitor none  (EIO workaround for Rock 5C 8812eu)
///   ip link set <if> up
///   iw dev <if> set power_save off  (best-effort)
///   verify: iw <if> info | grep "type monitor"
pub async fn set_monitor_mode_verified(iface: &str, max_retries: u32) -> bool {
    for attempt in 0..max_retries.max(1) {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500 * attempt as u64)).await;
        }
        if set_monitor_mode(iface).await && verify_monitor_mode(iface).await {
            return true;
        }
    }
    false
}

async fn set_monitor_mode(iface: &str) -> bool {
    // Defense in depth: never flip the operator's control path to monitor mode.
    // Bringing it down or changing its type would sever the only management link.
    if control_interface().await.as_deref() == Some(iface) {
        tracing::error!(iface = %iface, "monitor_mode_refused_control_iface");
        return false;
    }
    // Best-effort: release from NetworkManager.
    let _ = run_cmd("nmcli", &["dev", "set", iface, "managed", "no"]).await;
    // Bring down.
    if run_cmd("ip", &["link", "set", iface, "down"])
        .await
        .is_err()
    {
        return false;
    }
    // Monitor-form order is load-bearing on the RTL8812EU: `iw <if> set type
    // monitor` returns success on this chipset yet leaves the PHY pinned at the
    // muted txpower floor (readback -100 dBm, carrier down, every injection
    // sendmsg fails ENOBUFS), whereas `iw dev <if> set monitor none` fully
    // initialises the monitor PHY so it radiates. So the monitor-flags form is
    // primary (it is also the EIO workaround seen on other 8812eu builds); the
    // type form is the fallback for any adapter that rejects the flags form.
    let set_ok = if run_cmd("iw", &["dev", iface, "set", "monitor", "none"])
        .await
        .is_ok()
    {
        true
    } else {
        run_cmd("iw", &[iface, "set", "type", "monitor"])
            .await
            .is_ok()
    };
    if !set_ok {
        return false;
    }
    // Bring up.
    if run_cmd("ip", &["link", "set", iface, "up"]).await.is_err() {
        return false;
    }
    // Best-effort: disable power-save.
    let _ = run_cmd("iw", &["dev", iface, "set", "power_save", "off"]).await;
    true
}

async fn verify_monitor_mode(iface: &str) -> bool {
    get_interface_mode(iface).await.as_deref() == Some("monitor")
}

/// Parse the operating mode out of `iw <iface> info` output. Returns the value
/// after the `type ` line ("monitor" | "managed" | …), or `None` when the mode
/// cannot be determined. Pure helper, unit-tested independently of `iw`.
fn parse_interface_mode(info: &str) -> Option<String> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("type ") {
            let mode = rest.trim();
            if !mode.is_empty() {
                return Some(mode.to_string());
            }
        }
    }
    None
}

/// Return the interface operating mode ("monitor" | "managed" | …) by reading
/// `iw <iface> info`, or `None` when it cannot be read. Used to verify monitor
/// mode took effect and to detect when a scan stranded the iface in managed
/// mode (a managed-mode iface injects zero frames even though every command
/// "succeeded").
pub async fn get_interface_mode(iface: &str) -> Option<String> {
    let out = run_cmd_output("iw", &[iface, "info"]).await.ok()?;
    parse_interface_mode(&out)
}

/// Re-assert monitor mode and retune the current channel when a scan left the
/// iface in a non-monitor state.
///
/// `iw scan` can leave some RTL drivers in managed mode, which silently stops
/// `wfb_tx` injection while the link is still nominally up. After every scan the
/// hop loop calls this: when the observed mode is not monitor (and is readable),
/// it puts the iface back into monitor mode and retunes to `current_channel` so
/// `wfb_tx` keeps injecting on the channel it expects. When the mode is already
/// monitor (or unreadable) this is a cheap no-op.
pub async fn restore_monitor_if_needed(iface: &str, current_channel: u8) {
    match get_interface_mode(iface).await {
        Some(mode) if mode == "monitor" => {}
        None => {}
        Some(observed) => {
            tracing::warn!(iface, observed_mode = %observed, "monitor_restored_after_scan");
            if set_monitor_mode(iface).await {
                let _ = run_cmd(
                    "iw",
                    &[iface, "set", "channel", &current_channel.to_string()],
                )
                .await;
            }
        }
    }
}

/// Restore an interface to managed mode (used on shutdown or profile switch).
pub async fn set_managed_mode(iface: &str) {
    let _ = run_cmd("ip", &["link", "set", iface, "down"]).await;
    let _ = run_cmd("iw", &[iface, "set", "type", "managed"]).await;
    let _ = run_cmd("ip", &["link", "set", iface, "up"]).await;
    let _ = run_cmd("nmcli", &["dev", "set", iface, "managed", "yes"]).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_reads_type_line() {
        let info = "Interface wlan1\n\tifindex 5\n\twdev 0x1\n\ttype monitor\n\twiphy 0\n";
        assert_eq!(parse_interface_mode(info).as_deref(), Some("monitor"));
        let managed = "Interface wlan1\n\ttype managed\n";
        assert_eq!(parse_interface_mode(managed).as_deref(), Some("managed"));
    }

    #[test]
    fn parse_mode_missing_type_is_none() {
        assert!(parse_interface_mode("Interface wlan1\n\tifindex 5\n").is_none());
        assert!(parse_interface_mode("").is_none());
    }
}
