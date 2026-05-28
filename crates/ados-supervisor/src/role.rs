//! Ground-station mesh-role application at boot.
//!
//! Mirrors `role_manager.apply_role_on_boot_sync`: write the on-disk role
//! sentinel, then mask every mesh unit and unmask the ones for the active
//! role so a stray `systemctl start` cannot bring up a unit the node should
//! not run. Starting the role's units is left to the normal service lifecycle,
//! keeping start/stop sequencing owned in one place.

use std::path::Path;

use crate::config::VALID_ROLES;
use crate::systemctl;

/// Units gated by role, in dependency order (batman before the wfb side).
fn role_units(role: &str) -> &'static [&'static str] {
    match role {
        "relay" => &["ados-batman.service", "ados-wfb-relay.service"],
        "receiver" => &["ados-batman.service", "ados-wfb-receiver.service"],
        _ => &[], // direct (and any unexpected value) runs no mesh units
    }
}

const ALL_MESH_UNITS: [&str; 3] = [
    "ados-batman.service",
    "ados-wfb-relay.service",
    "ados-wfb-receiver.service",
];

/// Atomically write the role sentinel (`temp` + rename, 0o644).
fn write_role_file(path: &Path, role: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{role}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, path)
}

/// Apply the mask/unmask state for `role` at supervisor boot. Best-effort:
/// a sentinel write error is logged and the mask/unmask still runs.
pub async fn apply_role_on_boot(role: &str, role_path: &Path) {
    let role = if VALID_ROLES.contains(&role) {
        role
    } else {
        "direct"
    };

    if let Err(e) = write_role_file(role_path, role) {
        tracing::error!(error = %e, "role sentinel write failed");
    }

    for unit in ALL_MESH_UNITS {
        systemctl::mask(unit).await;
    }
    for unit in role_units(role) {
        systemctl::unmask(unit).await;
    }
    tracing::info!(role, "ground-station role applied at boot");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_units_match_the_catalog() {
        assert!(role_units("direct").is_empty());
        assert_eq!(
            role_units("relay"),
            &["ados-batman.service", "ados-wfb-relay.service"]
        );
        assert_eq!(
            role_units("receiver"),
            &["ados-batman.service", "ados-wfb-receiver.service"]
        );
        // unknown collapses to direct (no units)
        assert!(role_units("bogus").is_empty());
    }

    #[test]
    fn write_role_file_is_atomic_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh/role");
        write_role_file(&p, "relay").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "relay\n");
        // overwrite is clean
        write_role_file(&p, "direct").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "direct\n");
    }
}
