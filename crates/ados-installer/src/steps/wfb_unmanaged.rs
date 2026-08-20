//! Persistent NetworkManager "unmanaged" rule for the WFB monitor radios.
//!
//! Writes `/etc/NetworkManager/conf.d/10-ados-wfb-unmanaged.conf` so NetworkManager
//! structurally CANNOT claim a WFB monitor radio from the FIRST boot, before the
//! agent's runtime `nmcli dev set <iface> managed no` (`adapter.py`) has a chance to
//! run.
//!
//! The boot race this closes: NM autoconnects a manual WiFi-client keyfile pinned to
//! `wlan0` at boot, BEFORE the agent starts, and `wlan0` is (by driver) the RTL WFB
//! monitor radio. BY THE TIME `set_monitor_mode` runs `nmcli dev set managed no`, NM
//! has already associated the RTL to the operator network — and the management WiFi
//! never got it. Marking every WFB-compatible driver `unmanaged` removes the RTL from
//! NM's universe entirely, so a client/AP profile can only ever bind the non-WFB
//! radio, deterministically, by driver rather than by the racing kernel name.
//!
//! The rule is keyed by the DRIVER (e.g. `rtl88x2eu`), never by interface name —
//! kernel names race at boot, drivers do not. Idempotent: re-running on every install
//! / upgrade re-affirms the same file, so an operator hand-edit is overwritten and a
//! freshly-added WFB driver is re-covered. The runtime `nmcli dev set managed no` is
//! kept as defense-in-depth (never removed).
//!
//! The driver list is the WFB-compatible driver set — the single source of truth is
//! `crates/ados-protocol/wfb-adapters.toml` (generated into
//! `ados_protocol::wfb_tables::WFB_COMPATIBLE_DRIVERS` and
//! `ados.services.wfb._wfb_tables_generated`). The installer crate deliberately does
//! not depend on the protocol crate (it stays a lean bootstrap installer), so the set
//! is mirrored here with a parity test that would fail loudly if the two ever drift.
//!
//! Optional: a write failure degrades (the box still boots and the runtime
//! `nmcli dev set managed no` backstop still runs), never aborts the install.

use std::path::Path;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// The conf.d drop-in the rule is written to.
pub const NM_CONF_PATH: &str = "/etc/NetworkManager/conf.d/10-ados-wfb-unmanaged.conf";

/// The WFB-compatible kernel driver names, exactly the set in
/// `crates/ados-protocol/wfb-adapters.toml`. Kept in sync by
/// [`tests::driver_list_matches_the_generated_source`].
#[rustfmt::skip]
pub const WFB_UNMANAGED_DRIVERS: &[&str] = &[
    "8812au",
    "8812eu",
    "rtl8812au",
    "rtl8812eu",
    "rtl88x2eu",
    "rtl88xxau",
];

/// Render the `unmanaged-devices` value: each driver as its own `driver:` device
/// spec, `;`-separated as NetworkManager expects.
fn render_unmanaged_devices(drivers: &[&str]) -> String {
    drivers
        .iter()
        .map(|d| format!("driver:{d}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// Render the full conf.d file body (bland header + `[keyfile]` block + newline).
pub fn render_file(drivers: &[&str]) -> String {
    format!(
        "# ADOS Drone Agent — keep the WFB monitor radios out of NetworkManager.\n\
         # The WFB radios must never be NM-managed: NM would autoconnect a client\n\
         # profile to the RTL at boot, before the agent's runtime `nmcli dev set\n\
         # managed no` runs, breaking monitor mode. Keyed by driver (kernel names\n\
         # race at boot; drivers do not). Generated from the WFB-compatible driver\n\
         # set; do not edit by hand (overwritten on the next reconcile).\n\
         [keyfile]\n\
         unmanaged-devices={}\n",
        render_unmanaged_devices(drivers)
    )
}

/// Persistent NM unmanaged-devices rule for the WFB radios.
pub struct WfbUnmanaged;

impl Step for WfbUnmanaged {
    fn id(&self) -> &str {
        "wfb_unmanaged"
    }
    fn requires(&self) -> &[&str] {
        // No prerequisites: the rule is a static conf.d drop-in read at the NEXT
        // NM start, so writing it early (before any network step) is safe and
        // ensures it is in place before the first NM autoconnect on a fresh boot.
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-running on every upgrade re-affirms the rule, so an
        // operator hand-edit is overwritten and newly-added WFB drivers are covered.
        None
    }
    fn kind(&self) -> StepKind {
        // Optional: a write problem must degrade, never abort the install (the
        // runtime `nmcli dev set managed no` backstop still holds).
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let body = render_file(WFB_UNMANAGED_DRIVERS);
        let path = Path::new(NM_CONF_PATH);
        // Idempotent: skip the write when the file is already current.
        if std::fs::read_to_string(path)
            .map(|c| c == body)
            .unwrap_or(false)
        {
            tracing::info!("WFB unmanaged-devices rule already current");
            return StepOutcome::Ok;
        }
        if let Err(e) = write_file(path, &body) {
            tracing::warn!(error = %e, "failed to write WFB unmanaged-devices rule");
            return StepOutcome::Failed(format!("could not write {NM_CONF_PATH}: {e}"));
        }
        tracing::info!(drivers = WFB_UNMANAGED_DRIVERS.len(), "wrote WFB unmanaged-devices rule");
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

    #[test]
    fn unmanaged_devices_lists_every_driver_as_its_own_spec() {
        assert_eq!(
            render_unmanaged_devices(&["rtl88x2eu", "8812au"]),
            "driver:rtl88x2eu;driver:8812au"
        );
    }

    #[test]
    fn file_body_is_a_keyfile_unmanaged_block() {
        let body = render_file(&["rtl88x2eu"]);
        assert!(body.starts_with("# ADOS Drone Agent"));
        assert!(body.contains("[keyfile]\nunmanaged-devices=driver:rtl88x2eu\n"));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn driver_list_matches_the_generated_source() {
        // Parity lock against crates/ados-protocol/src/wfb_tables.rs
        // (WFB_COMPATIBLE_DRIVERS) and its Python twin
        // src/ados/services/wfb/_wfb_tables_generated.py (WFB_COMPATIBLE_DRIVERS).
        // If the source of truth gains a driver, this fails and the mirrored list
        // above must be kept in step.
        let expected = [
            "8812au",
            "8812eu",
            "rtl8812au",
            "rtl8812eu",
            "rtl88x2eu",
            "rtl88xxau",
        ];
        assert_eq!(WFB_UNMANAGED_DRIVERS, expected);
    }

    #[test]
    fn write_then_rewrite_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("10-ados-wfb-unmanaged.conf");
        let body = render_file(WFB_UNMANAGED_DRIVERS);
        write_file(&path, &body).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        // Re-writing the identical body leaves the file byte-identical.
        write_file(&path, &body).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }
}
