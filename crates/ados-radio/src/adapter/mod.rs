//! RTL8812EU adapter selection and monitor-mode setup.
//!
//! Mirrors `services/wfb/adapter.py`: scans network interfaces, identifies
//! RTL injection radios by USB VID/PID or driver name, denies AIC8800 and
//! brcmfmac management-wifi adapters, validates monitor mode with a 4× retry.
//!
//! All OS calls (iw, nmcli, ip) are Linux-only. On non-Linux hosts this
//! module compiles but `select_interface` always returns None.
//!
//! Grouped into focused sub-modules — `detect` (interface discovery +
//! classification + selection), `monitor` (monitor-mode set / verify / restore),
//! `reg` (regulatory domain set / verify / reconcile + channel gating), and
//! `txpower` (TX-power ramp + muted-readback policy) — and re-exported here so
//! the `ados_radio::adapter::*` contract callers use is unchanged. The shared
//! subprocess runners + the control-interface guard live in this barrel because
//! every sub-module relies on them.

mod detect;
mod monitor;
mod reg;
mod txpower;

pub use detect::{
    detect_and_select, detect_wfb_adapters, select_interface, usb_speed_degraded, SelectedAdapter,
    SelectionOutcome, WifiAdapterInfo,
};
pub use monitor::{
    get_interface_mode, restore_monitor_if_needed, set_managed_mode, set_monitor_mode_verified,
};
pub use reg::{
    assert_reg_ready, dfs_channels, enabled_channels, read_reg_status, reconcile_reg_domain,
    set_reg_domain, ReassertOutcome, RegError, RegStatus,
};
pub use txpower::{
    read_tx_power, set_tx_power, set_tx_power_modal, tx_power_ramp, MUTED_TX_POWER_DBM,
};

/// Parse the iface carrying the kernel default route out of `ip route` output.
/// Returns the first `dev <iface>` on a `default ...` line, or `None`.
pub(super) fn parse_default_route_iface(text: &str) -> Option<String> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() == Some(&"default") {
            if let Some(idx) = parts.iter().position(|p| *p == "dev") {
                if let Some(iface) = parts.get(idx + 1) {
                    return Some((*iface).to_string());
                }
            }
        }
    }
    None
}

/// Return the interface carrying the kernel default route, or `None`.
///
/// This is the operator's control path (the iface their SSH / Mission Control
/// session arrives over). The radio adapter selection and monitor-mode setup
/// must never touch it: bringing it down or flipping it to monitor mode would
/// sever the only management link with no fallback and strand the box. Best
/// effort — a missing default route (isolated rig) returns `None`.
pub(super) async fn control_interface() -> Option<String> {
    match run_cmd_output("ip", &["-4", "route", "show", "default"]).await {
        Ok(out) => parse_default_route_iface(&out),
        Err(()) => None,
    }
}

pub(super) async fn run_cmd(cmd: &str, args: &[&str]) -> Result<(), ()> {
    let status = tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|_| ())?;
    if status.success() {
        Ok(())
    } else {
        Err(())
    }
}

pub(super) async fn run_cmd_output(cmd: &str, args: &[&str]) -> Result<String, ()> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|_| ())?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_route_extracts_dev_iface() {
        let line = "default via 192.168.200.1 dev end1 proto dhcp metric 100";
        assert_eq!(parse_default_route_iface(line).as_deref(), Some("end1"));
        // A `dev`-first ordering (no `via`) still resolves.
        assert_eq!(
            parse_default_route_iface("default dev wwan0 scope link").as_deref(),
            Some("wwan0")
        );
    }

    #[test]
    fn parse_default_route_no_default_is_none() {
        // No default-route line in the output → None.
        assert!(parse_default_route_iface("10.0.0.0/24 dev eth0 scope link").is_none());
        assert!(parse_default_route_iface("").is_none());
    }
}
