//! The install-result contract and the closing-summary payload, assembled from
//! the probed node state after the step chain runs.

use std::path::Path;

use ados_term::exec;

use crate::ctx::Ctx;
use crate::env::{self, RESULT_PATH};
use crate::result::{now_iso8601_utc, FailureAccumulator, InstallResult};
use crate::ui::summary::SummaryData;

/// Assemble the renderer's closing-summary payload from the resolved context.
///
/// No URL is built here. The card's reach block is composed by
/// `ui::summary::console_urls`, which gates the `<host>.local` form on the
/// hostname actually being resolvable and otherwise lists the LAN addresses the
/// box owns. This function used to also carry a `setup_url` of
/// `http://<hostname>.local:8080/setup` — a name built with no such gate (so a
/// box named `localhost` was handed `localhost.local`) on a path the control
/// front does not serve. Nothing rendered it, which is the only reason it never
/// reached an operator; it is gone rather than repointed.
pub fn build_summary(status: &str, ctx: &Ctx) -> SummaryData {
    SummaryData {
        status: status.to_string(),
        version: installed_version(),
        profile: ctx.profile.clone(),
        board: board_id(),
        device_id: read_device_id(),
        hostname: env::read_hostname(),
        lan_ips: env::lan_ips(),
        paired: pairing_present(),
        ap_ssid: read_ap_ssid(&ctx.profile),
        ap_passphrase: read_ap_passphrase(),
        failed_steps: ctx.failures.failed.clone(),
        required_failures: ctx.failures.required.clone(),
        failure_reasons: ctx.failures.required_reasons.clone(),
        pending_reboot: ctx.pending_reboot.clone(),
    }
}

/// The AP's SSID, when this profile runs one.
///
/// Read out of the rendered hostapd config rather than rebuilt from a prefix,
/// so the card names the network the operator will actually see rather than
/// the one we would have chosen — an operator who renamed the AP was being
/// pointed at a network that no longer exists.
///
/// `None` when this profile runs no access point. Gated on the profile, not on
/// the passphrase file: file presence answers "has some manager run yet",
/// which on a fresh install is a race and on a drone is simply the wrong
/// question.
fn read_ap_ssid(profile: &str) -> Option<String> {
    if !matches!(profile, "ground_station" | "visor") {
        return None;
    }
    let body = std::fs::read_to_string(env::HOSTAPD_CONF_PATH).ok()?;
    body.lines()
        .find_map(|l| l.strip_prefix("ssid="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The AP passphrase the agent persisted, if any.
///
/// It is generated per unit rather than being one published default, so this
/// is the first moment an operator can learn it. Absent on a profile that runs
/// no access point.
fn read_ap_passphrase() -> Option<String> {
    let body = std::fs::read_to_string(env::AP_PASSPHRASE_PATH).ok()?;
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The node's device id (the full 12-hex id), or `unknown`.
fn read_device_id() -> String {
    Some(ados_protocol::identity::device_id(None))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether pairing material is present on disk.
fn pairing_present() -> bool {
    std::fs::metadata(env::PAIRING_JSON)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Print the install-result contract to stdout (for `--json`).
pub fn print_result_json() {
    if let Ok(body) = std::fs::read_to_string(RESULT_PATH) {
        print!("{body}");
    }
}

/// Copy the running installer to [`env::INSTALLED_INSTALLER`] (atomic, 0755).
/// Best-effort: a failure only means `ados uninstall` downloads the installer
/// instead, so it is logged, never fatal.
pub fn persist_installer() {
    let dest = Path::new(env::INSTALLED_INSTALLER);
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not locate the running installer to keep a copy");
            return;
        }
    };
    if exe == dest {
        return;
    }
    let written =
        std::fs::read(&exe).and_then(|bytes| env::write_atomic_durable(dest, &bytes, Some(0o755)));
    if let Err(e) = written {
        tracing::warn!(error = %e, dest = %dest.display(), "keeping a copy of the installer failed");
    }
}

/// Assemble + write the install-result contract with the real probed fields.
pub fn write_result(failures: &FailureAccumulator, status: &str, profile: &str) {
    let result = InstallResult {
        status: status.to_string(),
        version: installed_version(),
        profile: profile.to_string(),
        board: board_id(),
        kernel_release: kernel_release(),
        wfb_module_source: wfb_module_source(),
        failed_steps: failures.failed.clone(),
        required_failures: failures.required.clone(),
        ts: now_iso8601_utc(),
    };

    let path = Path::new(RESULT_PATH);
    if !result_path_writable(path) {
        tracing::warn!(path = %RESULT_PATH, "result path not writable; skipping result write (dev host?)");
        return;
    }
    if let Err(e) = result.write_atomic(path) {
        tracing::warn!(error = %e, path = %RESULT_PATH, "failed to write install result");
    }
}

/// The installed agent version, or `unknown` when the venv cannot report it.
fn installed_version() -> String {
    env::installed_agent_version().unwrap_or_else(|| "unknown".to_string())
}

/// The board id: the persisted override sentinel first, then the device-tree
/// model, else `unknown` (mirrors the bash `write_install_result` board read).
fn board_id() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/ados/board_override") {
        let v = s.trim().trim_matches('\0').trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/device-tree/model") {
        // The device-tree model is NUL-terminated; strip NULs + trim.
        let v = s.replace('\0', "");
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}

/// `uname -r`. Shells out via the exec primitive; `unknown` on any failure.
fn kernel_release() -> String {
    let res = exec::run("uname", &["-r"]);
    if res.success() {
        let v = res.stdout.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}

/// The WFB module source sentinel the driver step / driver script wrote
/// (`prebuilt` | `dkms`), or empty when no RTL adapter / driver is present.
fn wfb_module_source() -> String {
    std::fs::read_to_string("/run/ados/wfb-module-source")
        .map(|s| s.trim().trim_matches('\0').trim().to_string())
        .unwrap_or_default()
}

/// True when the result file's parent directory exists and is writable. Guards
/// the write so a dev host (no `/var/lib/ados`) does not error.
fn result_path_writable(path: &Path) -> bool {
    match path.parent() {
        Some(parent) => parent.is_dir() && is_writable(parent),
        None => false,
    }
}

#[cfg(target_os = "linux")]
fn is_writable(dir: &Path) -> bool {
    use nix::unistd::{access, AccessFlags};
    access(dir, AccessFlags::W_OK).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn is_writable(dir: &Path) -> bool {
    // Off Linux: probe by metadata read-only flag. Conservative — a false
    // negative only skips a result write on a dev host.
    std::fs::metadata(dir)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}
