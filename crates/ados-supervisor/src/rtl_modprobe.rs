//! RTL8812EU driver module-parameter generator for the operating-region posture.
//!
//! A self-managed-regulatory USB injection PHY (the RTL family) obeys its OWN
//! EEPROM-baked country regardless of the OS `iw reg set`, so relaxing the
//! software gate alone does not make a dongle baked with a restrictive country
//! radiate on the home channel. The PHY-level lever is the driver's module
//! parameters: with `rtw_regd_src=0` the driver uses its own private regdb and
//! ignores both the OS core domain and (with country `00`) the efuse country,
//! registering a worldwide channel plan that permits the home channel; the
//! regulatory power-LIMIT table (`rtw_tx_pwr_lmt_enable=0`) is switched off so the
//! legal per-region cap cannot clamp the home channel. The per-rate power
//! CALIBRATION table (`rtw_tx_pwr_by_rate`) defaults to `0` (OFF) so the home
//! channel runs at full driver power — bounded not by the efuse per-rate table but
//! by the software txpower clamp (`video.wfb.tx_power_dbm`), which stays armed for
//! brownout/thermal safety. With the table ON (`=1`) the efuse per-rate calibration
//! can cap the home channel BELOW the software clamp (down to the muted floor),
//! which starves the link; `=0` is the spec'd unrestricted value. It is overridable
//! to `1` (efuse per-rate PA linearization on) via `network.regulatory.tx_pwr_by_rate`
//! for adapters that genuinely need it — a bench A/B knob.
//!
//! This module renders the `options 8812eu ...` line for the active posture and
//! writes it to `/etc/modprobe.d/ados-rtl8812eu.conf`. Pinning a region instead
//! sets the driver's own regdb to that country with the power-limit table back on
//! for legal compliance in that jurisdiction.
//!
//! The generator is pure (unit-tested on every host); the file write + module
//! reload are Linux-only OS edges. The reload (`modprobe -r 8812eu` then
//! `modprobe 8812eu`) must NOT race the auto-pair/bind orchestrator — the caller
//! defers it while a bind window is open.
//!
//! NOTE: this is the agent-generated file. The vendored `realtek_88x2eu.conf`
//! must NOT be wired in — it uses `rtw_regd_src=1` (OS source), which still
//! applies the efuse country hint and so cannot lift a restrictive baked country.

/// Default per-rate PA-calibration table setting for the unrestricted posture.
/// `0` = OFF: the home channel runs at full driver power, bounded by the software
/// txpower clamp (`video.wfb.tx_power_dbm`, which stays armed for brownout/thermal
/// safety) rather than by the efuse per-rate table (which with `=1` can cap the
/// home channel below the clamp). Overridable via `network.regulatory.tx_pwr_by_rate`.
pub const DEFAULT_TX_PWR_BY_RATE: u8 = 0;

/// The operating-region posture the driver options encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModprobeMode {
    /// Worldwide plan, regulatory power-LIMIT table off. `tx_pwr_by_rate` is the
    /// per-rate PA-calibration table setting: `0` (default) = off/full driver power
    /// bounded by the software clamp; `1` = the efuse per-rate linearization on.
    Unrestricted { tx_pwr_by_rate: u8 },
    /// The driver's own regdb honours the pinned country, power-limit table on.
    /// Carries the uppercase ISO 3166-1 alpha-2 region code.
    Region(String),
}

/// The canonical modprobe config path the generator writes.
pub const MODPROBE_CONF_PATH: &str = "/etc/modprobe.d/ados-rtl8812eu.conf";

/// The RTL8812EU module name (the `options <module> ...` key + the reload target).
const MODULE_NAME: &str = "8812eu";

/// Render the `options 8812eu ...` line for the active posture. Pure.
///
/// - Unrestricted: `rtw_regd_src=0` (driver private regdb, efuse ignored) +
///   `rtw_country_code=00` (worldwide plan) + the regulatory power-LIMIT table off
///   (`rtw_tx_pwr_lmt_enable=0`, no legal cap) + the per-rate power CALIBRATION table
///   set from `tx_pwr_by_rate` (default `0` = off/full driver power bounded by the
///   software clamp; `1` = efuse per-rate linearization on).
/// - Region(R): `rtw_regd_src=0` + `rtw_country_code=R` (the driver regdb honours
///   R) + the power-limit table back on for legal compliance in R.
///
/// The region code is emitted verbatim (the caller validates/uppercases it).
pub fn render_modprobe_options(mode: &ModprobeMode) -> String {
    match mode {
        ModprobeMode::Unrestricted { tx_pwr_by_rate } => format!(
            "options {MODULE_NAME} rtw_regd_src=0 rtw_country_code=00 rtw_tx_pwr_lmt_enable=0 rtw_tx_pwr_by_rate={tx_pwr_by_rate}"
        ),
        ModprobeMode::Region(code) => format!(
            "options {MODULE_NAME} rtw_regd_src=0 rtw_country_code={code} rtw_tx_pwr_lmt_enable=1"
        ),
    }
}

/// Resolve the [`ModprobeMode`] from a `network.regulatory` config body. The
/// default (absent block, or `region` with no valid code) is unrestricted — the
/// permissive fresh-box posture. A `region` mode with a valid 2-char A-Z/0-9 code
/// pins that region. Pure so the resolution is unit-tested without the filesystem.
pub fn mode_from_config(text: &str) -> ModprobeMode {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        regulatory: Option<Reg>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Reg {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        tx_pwr_by_rate: Option<u8>,
    }
    let reg = serde_norway::from_str::<Raw>(text)
        .map(|r| r.network.regulatory)
        .unwrap_or_default();
    let Some(reg) = reg else {
        return ModprobeMode::Unrestricted {
            tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
        };
    };
    // The per-rate calibration override applies only to the unrestricted posture;
    // a pinned region uses the legal power-limit table instead.
    let tx_pwr_by_rate = reg.tx_pwr_by_rate.unwrap_or(DEFAULT_TX_PWR_BY_RATE);
    let is_region = reg
        .mode
        .as_deref()
        .map(|m| m.trim().eq_ignore_ascii_case("region"))
        .unwrap_or(false);
    if !is_region {
        return ModprobeMode::Unrestricted { tx_pwr_by_rate };
    }
    match reg.region.map(|r| r.trim().to_ascii_uppercase()) {
        Some(code) if is_valid_region_code(&code) => ModprobeMode::Region(code),
        // region mode without a valid code → unrestricted (permissive direction).
        _ => ModprobeMode::Unrestricted { tx_pwr_by_rate },
    }
}

/// True when `code` is a valid ISO 3166-1 alpha-2 / `00` world code: exactly two
/// ASCII uppercase-or-digit chars (checked against an already-uppercased input).
fn is_valid_region_code(code: &str) -> bool {
    code.len() == 2
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// True when the LIVE driver's parameters match what `mode` expects: the regd
/// source is always the driver's private regdb (`0`); the country is `00` for
/// unrestricted or the pinned region code. Pure so it is unit-tested on every
/// host. Used to detect a driver that loaded with its efuse country BEFORE the
/// options file was written (the file is current, but the loaded params are
/// stale — file-content equality alone misses this).
pub fn live_matches(live_country: &str, live_regd_src: &str, mode: &ModprobeMode) -> bool {
    if live_regd_src.trim() != "0" {
        return false;
    }
    let want = match mode {
        ModprobeMode::Unrestricted { .. } => "00",
        ModprobeMode::Region(code) => code.as_str(),
    };
    live_country.trim().eq_ignore_ascii_case(want)
}

/// Render the full file body (a header comment + the options line + trailing
/// newline). The header is bland and reader-facing — it describes what the file
/// does, not any internal planning artifact.
pub fn render_modprobe_file(mode: &ModprobeMode) -> String {
    format!(
        "# ADOS Drone Agent — RTL8812EU operating-region driver options.\n\
         # Generated by the agent from the network.regulatory config; do not edit\n\
         # by hand (changes are overwritten on the next reconcile).\n\
         {}\n",
        render_modprobe_options(mode)
    )
}

#[cfg(target_os = "linux")]
pub use linux::{apply, reconcile_from_config, reconcile_live_driver, reload_module};

/// Non-Linux build: no OS edges to drive, so the reconcile is an inert no-op.
/// Keeps the call site portable across the dev host and CI.
#[cfg(not(target_os = "linux"))]
pub fn reconcile_from_config(_reload_allowed: bool) {}

/// Non-Linux stub: nothing live to reconcile.
#[cfg(not(target_os = "linux"))]
pub fn reconcile_live_driver(_reload_allowed: bool) -> bool {
    true
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use ados_protocol::sidecar::write_atomic;
    use std::path::Path;

    /// The canonical agent config path the operating-region posture is read from.
    const CONFIG_YAML: &str = "/etc/ados/config.yaml";

    /// Read `network.regulatory` from the canonical config and write/reconcile the
    /// driver options file for the resolved posture. Best-effort: a missing config
    /// reads as unrestricted (the permissive default), and any write/reload failure
    /// is logged but never aborts the supervisor. `reload_allowed` is forwarded to
    /// [`apply`] so the caller can defer the live reload during a bind window.
    pub fn reconcile_from_config(reload_allowed: bool) {
        let mode = std::fs::read_to_string(CONFIG_YAML)
            .map(|t| super::mode_from_config(&t))
            .unwrap_or(ModprobeMode::Unrestricted {
                tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
            });
        match apply(&mode, reload_allowed) {
            Ok(true) => tracing::info!(?mode, "rtl_modprobe_options_updated"),
            Ok(false) => tracing::debug!(?mode, "rtl_modprobe_options_already_current"),
            Err(e) => tracing::warn!(error = %e, "rtl_modprobe_apply_failed"),
        }
    }

    /// Write the modprobe config for `mode` idempotently and reload the module so
    /// the new parameters take effect. Returns `Ok(true)` when the file content
    /// changed (and a reload was attempted), `Ok(false)` when it was already
    /// current (no reload). A write/reload failure is surfaced as an error so the
    /// caller can log it, but it is never fatal to the agent.
    ///
    /// `reload_allowed` gates the live `modprobe -r/modprobe` reload: the caller
    /// passes `false` while a bind window is open (the reload would race the bind
    /// orchestrator), so the new file is written but the reload is deferred to the
    /// next idle reconcile or a reboot. The on-disk file always wins on the next
    /// fresh module load regardless.
    pub fn apply(mode: &ModprobeMode, reload_allowed: bool) -> std::io::Result<bool> {
        let body = render_modprobe_file(mode);
        let path = Path::new(MODPROBE_CONF_PATH);
        let unchanged = std::fs::read_to_string(path)
            .map(|cur| cur == body)
            .unwrap_or(false);
        if unchanged {
            return Ok(false);
        }
        write_atomic(path, body.as_bytes(), 0o644)?;
        if reload_allowed {
            reload_module();
        } else {
            tracing::info!(
                note = "operating-region driver options written; reload deferred (bind active)",
                "rtl_modprobe_reload_deferred"
            );
        }
        Ok(true)
    }

    /// Reload the RTL8812EU module so a changed options file takes effect:
    /// `modprobe -r 8812eu` then `modprobe 8812eu`. Sequenced as two calls (not a
    /// single `modprobe -r` that auto-reprobes) so the unload completes before the
    /// reload, which avoids a half-applied parameter set. Best-effort: the RTL is
    /// never the management interface (the radio crate excludes the default-route
    /// iface), so a failed unload/reload degrades rather than severing a link.
    pub fn reload_module() {
        let unload = std::process::Command::new("modprobe")
            .args(["-r", MODULE_NAME])
            .status();
        match unload {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(code = ?s.code(), "rtl_modprobe_unload_failed"),
            Err(e) => tracing::warn!(error = %e, "rtl_modprobe_unload_error"),
        }
        let reload = std::process::Command::new("modprobe")
            .arg(MODULE_NAME)
            .status();
        match reload {
            Ok(s) if s.success() => {
                tracing::info!("rtl_modprobe_reloaded");
            }
            Ok(s) => tracing::warn!(code = ?s.code(), "rtl_modprobe_reload_failed"),
            Err(e) => tracing::warn!(error = %e, "rtl_modprobe_reload_error"),
        }
    }

    /// Live RTL8812EU module parameters in sysfs.
    const PARAM_COUNTRY: &str = "/sys/module/8812eu/parameters/rtw_country_code";
    const PARAM_REGD_SRC: &str = "/sys/module/8812eu/parameters/rtw_regd_src";

    /// Read a live module parameter, trimmed of whitespace and NULs. None when the
    /// module is not loaded or the parameter is absent.
    fn read_live_param(path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok().map(|s| {
            s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_string()
        })
    }

    /// Reconcile the LIVE driver against the configured posture. If the loaded
    /// module is on a different country / regd-source than the config wants — e.g.
    /// it loaded with its efuse country BEFORE the options file was written, so the
    /// file is current but the live parameters are stale (which `apply`'s
    /// file-content check misses) — reload the module so it re-reads the file.
    ///
    /// SAFETY: the reload removes + re-adds the module, so the caller MUST invoke
    /// this only when the RTL is NOT in use by a service — at the pre-bind point,
    /// right after the normal wfb unit is stopped (the radio crate excludes the
    /// management iface, so this is never the operator's link). A no-op in the
    /// common case (the live driver already matches → no reload), so the reload
    /// fires only when there is a real stale-driver problem. `reload_allowed=false`
    /// is a check-only. Returns whether the live driver matches the config after
    /// any reload.
    pub fn reconcile_live_driver(reload_allowed: bool) -> bool {
        let mode = std::fs::read_to_string(CONFIG_YAML)
            .map(|t| super::mode_from_config(&t))
            .unwrap_or(ModprobeMode::Unrestricted {
                tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
            });
        // Module not loaded → nothing live to reconcile; a fresh load picks up the
        // on-disk options.
        let (Some(country), Some(regd)) = (
            read_live_param(PARAM_COUNTRY),
            read_live_param(PARAM_REGD_SRC),
        ) else {
            return true;
        };
        if super::live_matches(&country, &regd, &mode) {
            return true;
        }
        tracing::warn!(live_country = %country, live_regd_src = %regd, ?mode, "rtl_live_driver_stale");
        if !reload_allowed {
            return false;
        }
        reload_module();
        let nc = read_live_param(PARAM_COUNTRY).unwrap_or_default();
        let nr = read_live_param(PARAM_REGD_SRC).unwrap_or_default();
        let ok = super::live_matches(&nc, &nr, &mode);
        if ok {
            tracing::info!("rtl_live_driver_reconciled");
        } else {
            tracing::warn!(live_country = %nc, live_regd_src = %nr, "rtl_live_driver_still_stale_after_reload");
        }
        ok
    }
}

#[cfg(test)]
mod tests;
