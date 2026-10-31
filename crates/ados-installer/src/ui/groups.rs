//! The install and uninstall checklists.
//!
//! The technical install steps are collapsed into ~10 human-readable groups so
//! the checklist reads at a glance; a group lights up when any of its member
//! steps starts and finalizes when all of them have a result.

use ados_term::progress::{GroupMap, ProgressSpec};

/// The install checklist groups. Each maps to one or more of the technical step
/// ids in the install chain; every chain step appears in exactly one group.
pub const INSTALL_GROUPS: GroupMap = &[
    ("Checking system", &["preflight", "purge_residue"]),
    ("Installing dependencies", &["deps"]),
    ("Agent runtime", &["venv_agent"]),
    ("Building radio stack", &["wfb_ng"]),
    ("Downloading components", &["fetch_binaries"]),
    ("Building drivers", &["dkms"]),
    (
        "Configuring",
        &["config_identity", "network_mac_pin", "rtl_regulatory"],
    ),
    ("Registering services", &["watchdog", "systemd"]),
    ("Starting agent", &["start"]),
    ("Verifying", &["health"]),
    ("Adding extensions", &["extensions"]),
];

/// The uninstall checklist groups (one per teardown phase; see
/// [`crate::uninstall::run_uninstall`]).
pub const UNINSTALL_GROUPS: GroupMap = &[
    ("Stopping services", &["stop_units"]),
    ("Reloading systemd", &["reload"]),
    ("Removing commands", &["commands"]),
    ("Removing files", &["files"]),
    ("Cleaning up", &["cleanup"]),
];

/// The install flow: groups plus wording.
pub const INSTALL_SPEC: ProgressSpec = ProgressSpec {
    groups: INSTALL_GROUPS,
    footer: "First install can take a few minutes. Safe to leave running.",
    rich_title: "ADOS Drone Agent · installing",
    skipped: "skipped",
    plain_skipped: "skipped",
    interrupted: "Install interrupted.",
};

/// The uninstall flow: groups plus wording.
pub const UNINSTALL_SPEC: ProgressSpec = ProgressSpec {
    groups: UNINSTALL_GROUPS,
    footer: "Removing the ADOS Drone Agent. This only takes a moment.",
    rich_title: "ADOS Drone Agent · removing",
    skipped: "skipped",
    plain_skipped: "skipped",
    interrupted: "Uninstall interrupted.",
};

#[cfg(test)]
mod tests {
    use super::*;
    use ados_term::progress::group_index_for_step;

    #[test]
    fn every_chain_step_maps_to_exactly_one_group() {
        let chain = [
            "preflight",
            "purge_residue",
            "deps",
            "venv_agent",
            "wfb_ng",
            "fetch_binaries",
            "dkms",
            "config_identity",
            "network_mac_pin",
            "rtl_regulatory",
            "watchdog",
            "systemd",
            "start",
            "health",
            "extensions",
        ];
        for step in chain {
            let hits = INSTALL_GROUPS
                .iter()
                .filter(|(_, steps)| steps.contains(&step))
                .count();
            assert_eq!(hits, 1, "step {step} must map to exactly one group");
        }
        // And no group references a step outside the chain.
        for (_, steps) in INSTALL_GROUPS {
            for s in *steps {
                assert!(chain.contains(s), "group references unknown step {s}");
            }
        }
    }

    #[test]
    fn group_lookup_resolves() {
        assert_eq!(group_index_for_step(INSTALL_GROUPS, "deps"), Some(1));
        assert_eq!(
            group_index_for_step(INSTALL_GROUPS, "health"),
            Some(INSTALL_GROUPS.len() - 2)
        );
        assert_eq!(
            group_index_for_step(INSTALL_GROUPS, "extensions"),
            Some(INSTALL_GROUPS.len() - 1)
        );
        assert_eq!(group_index_for_step(INSTALL_GROUPS, "nope"), None);
        // Uninstall groups resolve against their own map.
        assert_eq!(group_index_for_step(UNINSTALL_GROUPS, "cleanup"), Some(4));
        assert_eq!(group_index_for_step(UNINSTALL_GROUPS, "deps"), None);
    }
}
