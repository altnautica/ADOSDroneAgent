//! Linux OS edges for the WiFi self-heal watchdog: candidate enumeration, the
//! gateway / neighbor reachability probes, and the NetworkManager down/up
//! re-association. All read-only except the best-effort `reactivate_connection`.
//! The pure parsing + classification these drive live in `decision`.

#![cfg(target_os = "linux")]

use std::time::Duration;

use super::decision::{
    iface_is_managed_candidate, looks_like_access_point, parse_active_wifi_connections,
    parse_gateway, parse_neighbor_reachable, WifiConnection,
};

/// Enumerate onboard managed-WiFi candidates: active `802-11-wireless`
/// connections from nmcli, filtered to interfaces that run a non-injection
/// driver and are in managed/station mode (so the monitor-mode radio adapter is
/// excluded three ways: it is not usually a managed connection, it runs a WFB
/// driver, and it is in monitor mode).
pub(super) async fn enumerate_candidates() -> Vec<WifiConnection> {
    let Some(terse) = run_cmd_output(
        "nmcli",
        &["-t", "-f", "NAME,TYPE,DEVICE,STATE", "connection", "show"],
    )
    .await
    else {
        return Vec::new();
    };
    let conns = parse_active_wifi_connections(&terse, looks_like_access_point);
    let mut out = Vec::new();
    for conn in conns {
        let driver = driver_name(&conn.iface).await;
        let mode = interface_mode(&conn.iface).await;
        if iface_is_managed_candidate(&driver, mode.as_deref()) {
            out.push(conn);
        }
    }
    out
}

/// Read the kernel driver bound to an interface from
/// `/sys/class/net/<if>/device/driver` (a symlink ending with the driver name).
/// Empty when it cannot be read.
async fn driver_name(iface: &str) -> String {
    let link = format!("/sys/class/net/{}/device/driver", iface);
    tokio::fs::read_link(&link)
        .await
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// Read an interface's operating mode ("managed" | "monitor" | …) from
/// `iw <iface> info`, or `None` when it cannot be read.
async fn interface_mode(iface: &str) -> Option<String> {
    let out = run_cmd_output("iw", &[iface, "info"]).await?;
    for line in out.lines() {
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

/// Return the default-route gateway for an interface, or `None`. Read-only.
pub(super) async fn default_gateway_for_iface(iface: &str) -> Option<String> {
    let out = run_cmd_output("ip", &["-4", "route", "show", "default", "dev", iface]).await?;
    parse_gateway(&out)
}

/// Probe whether the gateway is reachable from an interface via the kernel
/// neighbor (ARP) table: a single, cheap, read-only `ip neighbor show <gw> dev
/// <iface>`. Never sends traffic on or reconfigures the radio interface. A
/// missing or INCOMPLETE/FAILED entry means the gateway does not answer ARP (the
/// dead-data-path condition).
pub(super) async fn gateway_reachable(iface: &str, gateway: &str) -> bool {
    match run_cmd_output("ip", &["neighbor", "show", gateway, "dev", iface]).await {
        Some(out) => parse_neighbor_reachable(&out),
        None => false,
    }
}

/// Brief pause between the connection down and up so the kernel fully clears the
/// old association + regulatory state before the fresh one forms.
const REASSOC_SETTLE: Duration = Duration::from_millis(500);

/// Re-activate one NetworkManager connection with the proven down/up cycle. The
/// `down` gracefully tears the association + IP stack down; the `up` re-forms it
/// under the now-settled regulatory domain. Both calls are best-effort: a
/// connection that was not up returns non-zero on `down`, which is fine — the
/// `up` still rebuilds it.
pub(super) async fn reactivate_connection(name: &str) {
    let _ = run_cmd("nmcli", &["connection", "down", name]).await;
    tokio::time::sleep(REASSOC_SETTLE).await;
    if !run_cmd("nmcli", &["connection", "up", name]).await {
        tracing::warn!(connection = %name, "wifi_selfheal_up_failed");
    } else {
        tracing::info!(connection = %name, "wifi_selfheal_reactivated");
    }
}

/// True when the `nmcli` binary is on PATH.
pub(super) async fn nmcli_available() -> bool {
    run_cmd("sh", &["-c", "command -v nmcli"]).await
}

/// Run a command, returning true on a zero exit. stdout/stderr are discarded.
async fn run_cmd(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout as a string, or `None` when it could not be
/// run. A non-zero exit still returns whatever was written to stdout.
async fn run_cmd_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}
