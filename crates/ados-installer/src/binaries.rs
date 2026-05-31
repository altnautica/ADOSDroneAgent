//! The prebuilt-binary catalog.
//!
//! Each Rust service ships as an `<service>-aarch64` asset attached to a
//! per-service prebuilt release tag. The fetch step downloads the assets for
//! the active profile, verifies them, and drops each at
//! `/opt/ados/bin/<service>`. A `Hard` gate means a missing/failed binary
//! fails the install; a `BestEffort` gate degrades it. Multiple services can
//! share one release tag (the HID and display binaries are built and published
//! together), so the table maps service → tag, not the reverse.

/// Whether a missing prebuilt binary is fatal (`Hard`) or degrading
/// (`BestEffort`) to the install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The install fails without this binary.
    Hard,
    /// The install degrades but proceeds without this binary.
    BestEffort,
}

/// One prebuilt service binary: where it comes from, where it lands, how hard
/// its absence is, and which profiles need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrebuiltBinary {
    /// Service name (also the installed binary basename).
    pub service: &'static str,
    /// GitHub release asset name (`<service>-aarch64`).
    pub asset: &'static str,
    /// The release tag the asset is attached to.
    pub release_tag: &'static str,
    /// Install destination.
    pub dest: &'static str,
    /// Hard vs best-effort gate.
    pub gate: Gate,
    /// Profiles that need this binary (`drone` and/or `ground_station`).
    pub profiles: &'static [&'static str],
}

/// Profile constants — kept as slices so the const table can reference them.
const BOTH: &[&str] = &["drone", "ground_station"];
const DRONE: &[&str] = &["drone"];
const GROUND: &[&str] = &["ground_station"];

/// The full catalog of 14 prebuilt service binaries.
///
/// Gate rationale: the agent cannot do its job without the orchestrator
/// (`ados-supervisor`), the video pipeline (`ados-video`), the cloud-relay
/// transport (`ados-cloud`), or the vision host (`ados-vision`), so those four
/// are `Hard`. Everything else degrades to best-effort: the agent still comes
/// up and reports the missing capability via the install result.
pub const PREBUILT: &[PrebuiltBinary] = &[
    PrebuiltBinary {
        service: "ados-tui",
        asset: "ados-tui-aarch64",
        release_tag: "prebuilt-tui",
        dest: "/opt/ados/bin/ados-tui",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    PrebuiltBinary {
        service: "ados-supervisor",
        asset: "ados-supervisor-aarch64",
        release_tag: "prebuilt-supervisor",
        dest: "/opt/ados/bin/ados-supervisor",
        gate: Gate::Hard,
        profiles: BOTH,
    },
    PrebuiltBinary {
        service: "ados-mavlink-router",
        asset: "ados-mavlink-router-aarch64",
        release_tag: "prebuilt-mavlink-router",
        dest: "/opt/ados/bin/ados-mavlink-router",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    PrebuiltBinary {
        service: "ados-radio",
        asset: "ados-radio-aarch64",
        release_tag: "prebuilt-radio",
        dest: "/opt/ados/bin/ados-radio",
        gate: Gate::BestEffort,
        profiles: DRONE,
    },
    PrebuiltBinary {
        service: "ados-video",
        asset: "ados-video-aarch64",
        release_tag: "prebuilt-video",
        dest: "/opt/ados/bin/ados-video",
        gate: Gate::Hard,
        profiles: DRONE,
    },
    PrebuiltBinary {
        service: "ados-plugin-host",
        asset: "ados-plugin-host-aarch64",
        release_tag: "prebuilt-plugin-host",
        dest: "/opt/ados/bin/ados-plugin-host",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    PrebuiltBinary {
        service: "ados-cloud",
        asset: "ados-cloud-aarch64",
        release_tag: "prebuilt-cloud",
        dest: "/opt/ados/bin/ados-cloud",
        gate: Gate::Hard,
        profiles: BOTH,
    },
    PrebuiltBinary {
        service: "ados-groundlink",
        asset: "ados-groundlink-aarch64",
        release_tag: "prebuilt-groundlink",
        dest: "/opt/ados/bin/ados-groundlink",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-net",
        asset: "ados-net-aarch64",
        release_tag: "prebuilt-net",
        dest: "/opt/ados/bin/ados-net",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-pic",
        asset: "ados-pic-aarch64",
        release_tag: "prebuilt-hid",
        dest: "/opt/ados/bin/ados-pic",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-input",
        asset: "ados-input-aarch64",
        release_tag: "prebuilt-hid",
        dest: "/opt/ados/bin/ados-input",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-display",
        asset: "ados-display-aarch64",
        release_tag: "prebuilt-display",
        dest: "/opt/ados/bin/ados-display",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-display-probe",
        asset: "ados-display-probe-aarch64",
        release_tag: "prebuilt-display",
        dest: "/opt/ados/bin/ados-display-probe",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-vision",
        asset: "ados-vision-aarch64",
        release_tag: "prebuilt-vision",
        dest: "/opt/ados/bin/ados-vision",
        gate: Gate::Hard,
        profiles: DRONE,
    },
];

/// The subset of the catalog needed by `profile` (`drone` | `ground_station`).
pub fn for_profile(profile: &str) -> Vec<&'static PrebuiltBinary> {
    PREBUILT
        .iter()
        .filter(|b| b.profiles.contains(&profile))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_fourteen_entries() {
        assert_eq!(PREBUILT.len(), 14);
    }

    #[test]
    fn exactly_four_hard_and_they_are_the_right_ones() {
        let hard: Vec<&str> = PREBUILT
            .iter()
            .filter(|b| b.gate == Gate::Hard)
            .map(|b| b.service)
            .collect();
        assert_eq!(hard.len(), 4, "hard gates: {hard:?}");
        for svc in ["ados-supervisor", "ados-video", "ados-cloud", "ados-vision"] {
            assert!(hard.contains(&svc), "{svc} must be a Hard gate");
        }
    }

    #[test]
    fn asset_matches_service_aarch64() {
        for b in PREBUILT {
            assert_eq!(b.asset, format!("{}-aarch64", b.service).as_str());
        }
    }

    #[test]
    fn dest_is_under_bin_dir() {
        for b in PREBUILT {
            assert_eq!(b.dest, format!("/opt/ados/bin/{}", b.service).as_str());
        }
    }

    #[test]
    fn pic_and_input_share_the_hid_release_tag() {
        let tag = |svc: &str| {
            PREBUILT
                .iter()
                .find(|b| b.service == svc)
                .map(|b| b.release_tag)
                .unwrap()
        };
        assert_eq!(tag("ados-pic"), "prebuilt-hid");
        assert_eq!(tag("ados-input"), "prebuilt-hid");
        assert_eq!(tag("ados-display"), "prebuilt-display");
        assert_eq!(tag("ados-display-probe"), "prebuilt-display");
    }

    #[test]
    fn profile_filter_excludes_other_profile() {
        let drone = for_profile("drone");
        assert!(drone.iter().any(|b| b.service == "ados-video"));
        assert!(!drone.iter().any(|b| b.service == "ados-groundlink"));

        let ground = for_profile("ground_station");
        assert!(ground.iter().any(|b| b.service == "ados-groundlink"));
        assert!(!ground.iter().any(|b| b.service == "ados-video"));

        // Shared services appear in both.
        assert!(drone.iter().any(|b| b.service == "ados-supervisor"));
        assert!(ground.iter().any(|b| b.service == "ados-supervisor"));
    }
}
