//! RTL8812EU operating-region driver options at install time.
//!
//! Writes the initial `/etc/modprobe.d/ados-rtl8812eu.conf` from the operating-
//! region posture seeded in `config.yaml` (unrestricted by default), so a fresh
//! box is permissive on the FIRST module load with zero operator input. Without
//! this, a self-managed RTL dongle baked with a restrictive country would obey
//! that country on first boot regardless of the software gate, and the home
//! channel would be capped.
//!
//! The options line is the PHY-level lever: with `rtw_regd_src=0` the driver uses
//! its own private regdb and ignores both the OS core domain and (with country
//! `00`) the efuse country, registering a worldwide plan that permits the home
//! channel; the regulatory power-LIMIT table (`rtw_tx_pwr_lmt_enable=0`) is off so
//! the legal per-region cap cannot clamp the home channel. The per-rate power
//! CALIBRATION table (`rtw_tx_pwr_by_rate`) defaults to `0` (OFF) so the home
//! channel runs at full driver power — bounded not by the efuse per-rate table but
//! by the software txpower clamp (`video.wfb.tx_power_dbm`), which stays armed for
//! brownout/thermal safety. With the table ON (`=1`) the efuse per-rate calibration
//! can cap the home channel BELOW the software clamp (down to the muted floor),
//! starving the link. It is overridable to `1` via `network.regulatory.tx_pwr_by_rate`
//! (a bench A/B knob). Pinning a region instead turns the regulatory limit table
//! back on.
//!
//! The vendored `realtek_88x2eu.conf` template is deliberately NOT wired in: it
//! uses `rtw_regd_src=1` (OS source), which still applies the efuse country hint
//! and so cannot lift a restrictive baked country. This step generates ours.
//!
//! Optional: a write failure degrades (the box still comes up; the supervisor
//! re-reconciles the options file at every start). Runs after `dkms` so the
//! module exists, and after `config_identity` so `config.yaml` has been written.

use std::path::Path;

use serde::Deserialize;

use crate::ctx::Ctx;
use crate::env::CONFIG_YAML;
use crate::graph::{Step, StepKind, StepOutcome};

/// The modprobe config path the step writes.
const MODPROBE_CONF_PATH: &str = "/etc/modprobe.d/ados-rtl8812eu.conf";

/// The slice of `config.yaml` this step reads. Everything is optional so a config
/// without a `network.regulatory` block resolves to the unrestricted default.
#[derive(Debug, Deserialize, Default)]
struct RootView {
    #[serde(default)]
    network: Option<NetworkView>,
}
#[derive(Debug, Deserialize, Default)]
struct NetworkView {
    #[serde(default)]
    regulatory: Option<RegulatoryView>,
}
#[derive(Debug, Deserialize, Default)]
struct RegulatoryView {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    tx_pwr_by_rate: Option<u8>,
}

/// Default per-rate PA-calibration table setting for the unrestricted posture.
/// `0` = OFF: full driver power on the home channel, bounded by the software
/// txpower clamp (which stays armed for brownout/thermal safety) rather than the
/// efuse per-rate table. Must match the supervisor's `DEFAULT_TX_PWR_BY_RATE` so
/// the install-time seed is byte-identical to the runtime reconcile.
const DEFAULT_TX_PWR_BY_RATE: u8 = 0;

/// The resolved operating-region posture.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Posture {
    Unrestricted { tx_pwr_by_rate: u8 },
    Region(String),
}

/// Resolve the posture from a `network.regulatory` config body. Default
/// (absent block, or a `region` mode with no valid code) is unrestricted — the
/// permissive fresh-box posture. Pure so it is unit-tested without the filesystem.
fn resolve_posture(text: &str) -> Posture {
    let reg = serde_norway::from_str::<RootView>(text)
        .ok()
        .and_then(|r| r.network)
        .and_then(|n| n.regulatory);
    let Some(reg) = reg else {
        return Posture::Unrestricted {
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
        return Posture::Unrestricted { tx_pwr_by_rate };
    }
    match reg.region.map(|r| r.trim().to_ascii_uppercase()) {
        Some(code) if is_valid_region_code(&code) => Posture::Region(code),
        _ => Posture::Unrestricted { tx_pwr_by_rate },
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

/// Render the `options 8812eu ...` line for the posture. Pure. Byte-identical to
/// the supervisor's generator so the install-time seed matches the runtime
/// reconcile exactly.
fn render_options(posture: &Posture) -> String {
    match posture {
        Posture::Unrestricted { tx_pwr_by_rate } => format!(
            "options 8812eu rtw_regd_src=0 rtw_country_code=00 rtw_tx_pwr_lmt_enable=0 rtw_tx_pwr_by_rate={tx_pwr_by_rate}"
        ),
        Posture::Region(code) => format!(
            "options 8812eu rtw_regd_src=0 rtw_country_code={code} rtw_tx_pwr_lmt_enable=1"
        ),
    }
}

/// Render the full modprobe file body (bland header + options line + newline).
fn render_file(posture: &Posture) -> String {
    format!(
        "# ADOS Drone Agent — RTL8812EU operating-region driver options.\n\
         # Generated by the agent from the network.regulatory config; do not edit\n\
         # by hand (changes are overwritten on the next reconcile).\n\
         {}\n",
        render_options(posture)
    )
}

/// RTL8812EU operating-region driver-options seed.
pub struct RtlRegulatory;

impl Step for RtlRegulatory {
    fn id(&self) -> &str {
        "rtl_regulatory"
    }
    fn requires(&self) -> &[&str] {
        // After config (the posture is read from config.yaml) and after the
        // driver build (the module the options apply to exists).
        &["config_identity", "dkms"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-running on every upgrade re-affirms the options file,
        // so an operator's later region change is re-seeded idempotently.
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: a write problem must degrade, never abort the install (the
        // supervisor re-reconciles the file at every start).
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let posture = std::fs::read_to_string(CONFIG_YAML)
            .map(|t| resolve_posture(&t))
            .unwrap_or(Posture::Unrestricted {
                tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
            });
        let body = render_file(&posture);
        let path = Path::new(MODPROBE_CONF_PATH);
        // Idempotent: skip the write when the file is already current.
        if std::fs::read_to_string(path)
            .map(|c| c == body)
            .unwrap_or(false)
        {
            tracing::info!(?posture, "RTL operating-region options already current");
            return StepOutcome::Ok;
        }
        if let Err(e) = write_file(path, &body) {
            tracing::warn!(error = %e, "failed to write RTL operating-region options");
            return StepOutcome::Failed(format!("could not write {MODPROBE_CONF_PATH}: {e}"));
        }
        tracing::info!(?posture, "wrote RTL operating-region driver options");
        StepOutcome::Ok
    }
}

/// Write `body` to `path` atomically (tmp + rename), creating the parent dir.
fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default unrestricted posture (per-rate calibration off), for tests.
    fn unr() -> Posture {
        Posture::Unrestricted {
            tx_pwr_by_rate: DEFAULT_TX_PWR_BY_RATE,
        }
    }

    #[test]
    fn default_config_resolves_to_unrestricted() {
        assert_eq!(resolve_posture(""), unr());
        assert_eq!(resolve_posture("agent:\n  name: x\n"), unr());
        // The seed config_identity writes has no network.regulatory block, so a
        // fresh box is permissive.
        assert_eq!(resolve_posture("video:\n  wfb:\n    channel: 149\n"), unr());
    }

    #[test]
    fn region_config_resolves_to_pinned_region() {
        assert_eq!(
            resolve_posture("network:\n  regulatory:\n    mode: region\n    region: in\n"),
            Posture::Region("IN".to_string())
        );
    }

    #[test]
    fn region_without_valid_code_is_unrestricted() {
        assert_eq!(
            resolve_posture("network:\n  regulatory:\n    mode: region\n"),
            unr()
        );
        assert_eq!(
            resolve_posture("network:\n  regulatory:\n    mode: region\n    region: USA\n"),
            unr()
        );
    }

    #[test]
    fn unrestricted_options_line_is_exact() {
        // Default per-rate calibration table OFF (=0): full driver power bounded by
        // the software txpower clamp, not the efuse table. Must stay byte-identical
        // to the supervisor's generator.
        assert_eq!(
            render_options(&unr()),
            "options 8812eu rtw_regd_src=0 rtw_country_code=00 rtw_tx_pwr_lmt_enable=0 rtw_tx_pwr_by_rate=0"
        );
        assert_eq!(DEFAULT_TX_PWR_BY_RATE, 0);
    }

    #[test]
    fn tx_pwr_by_rate_override_threads_through() {
        // The bench A/B knob overrides the default under the unrestricted posture.
        assert_eq!(
            resolve_posture("network:\n  regulatory:\n    tx_pwr_by_rate: 1\n"),
            Posture::Unrestricted { tx_pwr_by_rate: 1 }
        );
        assert!(render_options(&Posture::Unrestricted { tx_pwr_by_rate: 1 })
            .contains("rtw_tx_pwr_by_rate=1"));
    }

    #[test]
    fn region_options_line_is_exact() {
        assert_eq!(
            render_options(&Posture::Region("IN".to_string())),
            "options 8812eu rtw_regd_src=0 rtw_country_code=IN rtw_tx_pwr_lmt_enable=1"
        );
    }

    #[test]
    fn neither_options_line_uses_the_os_regdb_source() {
        // rtw_regd_src=1 (OS source) keeps the efuse hint and cannot lift a baked
        // country; the seed never emits it.
        assert!(!render_options(&unr()).contains("rtw_regd_src=1"));
        assert!(!render_options(&Posture::Region("US".to_string())).contains("rtw_regd_src=1"));
    }

    #[test]
    fn file_body_carries_header_and_options() {
        let body = render_file(&unr());
        assert!(body.starts_with("# ADOS Drone Agent"));
        assert!(body.contains(&render_options(&unr())));
        assert!(body.ends_with('\n'));
    }
}
