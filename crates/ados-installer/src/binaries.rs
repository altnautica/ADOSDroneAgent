//! The prebuilt-binary catalog.
//!
//! Each service ships as an `<asset>` attached to a per-service prebuilt
//! release tag. The fetch step downloads the assets for the active profile,
//! verifies them, and drops each at its destination (the `ados-*` services
//! under `/opt/ados/bin/<service>`; a mirrored third-party relay under the
//! system bin dir). A `Hard` gate means a missing/failed binary fails the
//! install; a `BestEffort` gate degrades it. Multiple services can share one
//! release tag (the HID and display binaries are built and published
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
/// The workstation profile (a GPU box / Mac / spare box that reconstructs +
/// serves perception offload). Distinct from the SBC profiles.
const WORKSTATION: &[&str] = &["workstation"];
/// Every profile, including the workstation node. Used for the profile-agnostic core
/// services every node needs (orchestrator, cloud relay, control front,
/// logging, TUI) so a `--profile workstation` install fetches them too.
const ANY: &[&str] = &["drone", "ground_station", "workstation"];

/// The full catalog of prebuilt service binaries.
///
/// Gate rationale: the agent cannot do its job without the orchestrator
/// (`ados-supervisor`), the MAVLink router (`ados-mavlink-router`), the video
/// pipeline (`ados-video`), the cloud-relay transport (`ados-cloud`), or the
/// vision host (`ados-vision`), so those are `Hard`. The router is the sole
/// command-and-control path to the flight controller — the packaged Python
/// MAVLink service it replaced is gone, so a missing router leaves the Core
/// MAVLink unit crash-looping with no FC telemetry, arming, or GCS link. A
/// fetch miss must therefore FAIL the install rather than report it healthy.
/// Everything else degrades to best-effort: the agent still comes up and
/// reports the missing capability via the install result.
pub const PREBUILT: &[PrebuiltBinary] = &[
    PrebuiltBinary {
        service: "ados-tui",
        asset: "ados-tui-aarch64",
        release_tag: "prebuilt-tui",
        dest: "/opt/ados/bin/ados-tui",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-supervisor",
        asset: "ados-supervisor-aarch64",
        release_tag: "prebuilt-supervisor",
        dest: "/opt/ados/bin/ados-supervisor",
        gate: Gate::Hard,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-mavlink-router",
        asset: "ados-mavlink-router-aarch64",
        release_tag: "prebuilt-mavlink-router",
        dest: "/opt/ados/bin/ados-mavlink-router",
        // The sole command-and-control path: the Core MAVLink unit execs this
        // binary unconditionally and has no Python fallback. Hard on both
        // profiles so a fetch miss aborts the install instead of shipping a
        // unit that crash-loops with no FC link.
        gate: Gate::Hard,
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
        profiles: ANY,
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
    // The local logging and telemetry store. Best-effort: a missing store
    // degrades recordkeeping (the agent falls back to journald) without
    // aborting the install. The unit ships deployed-but-not-enabled, so the
    // store stays off until it is explicitly turned on through the cutover
    // tooling — a controlled rollout rather than an unconditional default.
    PrebuiltBinary {
        service: "ados-logd",
        asset: "ados-logd-aarch64",
        release_tag: "prebuilt-logd",
        dest: "/opt/ados/bin/ados-logd",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    // The native HTTP control surface. Best-effort and opt-in: it ships disabled
    // (the GCS uses the FastAPI surface), so a missing binary degrades nothing.
    // It is fetched and placed so `ados rust enable control` works on demand; the
    // unit stays disabled until the operator turns it on.
    PrebuiltBinary {
        service: "ados-control",
        asset: "ados-control-aarch64",
        release_tag: "prebuilt-control",
        dest: "/opt/ados/bin/ados-control",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    // The GPIO-output service (status buzzer / LED). Best-effort and opt-in: it
    // ships disabled (the unit's ExecStart guard execs /bin/true until the
    // operator drops the enable marker), so a missing binary degrades nothing. It
    // is fetched and placed so enabling it works on demand. Cross-profile: a
    // header GPIO can drive an indicator on an air or a ground node.
    PrebuiltBinary {
        service: "ados-gpio",
        asset: "ados-gpio-aarch64",
        release_tag: "prebuilt-gpio",
        dest: "/opt/ados/bin/ados-gpio",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    // The video relay the pipeline streams through. It is a mirrored
    // third-party binary rather than an `ados-*` service, so it lands in the
    // system bin dir. Best-effort: a missing relay degrades video without
    // aborting the install (the health gate verifies its presence separately).
    PrebuiltBinary {
        service: "mediamtx",
        asset: "mediamtx-aarch64",
        release_tag: "prebuilt-mediamtx",
        dest: "/usr/local/bin/mediamtx",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    // The compute reconstructor/offload daemon. Best-effort so a workstation host
    // whose CPU arch the prebuilt pipeline does not cover (a GPU box / Mac is
    // typically x86_64/arm64-macOS, while the catalog ships aarch64) degrades +
    // reports rather than failing the install; that host builds from source.
    PrebuiltBinary {
        service: "ados-compute",
        asset: "ados-compute-aarch64",
        release_tag: "prebuilt-compute",
        dest: "/opt/ados/bin/ados-compute",
        gate: Gate::BestEffort,
        profiles: WORKSTATION,
    },
];

/// The subset of the catalog needed by `profile`
/// (`drone` | `ground_station` | `workstation`).
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
    fn catalog_has_nineteen_entries() {
        assert_eq!(PREBUILT.len(), 19);
    }

    #[test]
    fn workstation_profile_fetches_the_cores_and_the_compute_daemon() {
        let svcs: Vec<&str> = for_profile("workstation")
            .iter()
            .map(|b| b.service)
            .collect();
        // The workstation node is a full agent: the orchestrator, cloud relay,
        // control front (LAN pairing), logging, and TUI, plus the compute daemon.
        for svc in [
            "ados-supervisor",
            "ados-cloud",
            "ados-control",
            "ados-logd",
            "ados-tui",
            "ados-compute",
        ] {
            assert!(
                svcs.contains(&svc),
                "workstation profile must fetch {svc}: {svcs:?}"
            );
        }
        // It does NOT fetch the SBC-only flight/radio/video surfaces.
        for svc in [
            "ados-mavlink-router",
            "ados-video",
            "ados-vision",
            "ados-radio",
        ] {
            assert!(
                !svcs.contains(&svc),
                "workstation profile must NOT fetch {svc}: {svcs:?}"
            );
        }
        // The compute daemon degrades (build-from-source on an uncovered arch).
        let compute = PREBUILT
            .iter()
            .find(|b| b.service == "ados-compute")
            .expect("ados-compute in the catalog");
        assert_eq!(compute.gate, Gate::BestEffort);
        assert_eq!(compute.release_tag, "prebuilt-compute");
    }

    #[test]
    fn gpio_is_best_effort_on_both_profiles() {
        let gpio = PREBUILT
            .iter()
            .find(|b| b.service == "ados-gpio")
            .expect("ados-gpio must be in the catalog");
        // The GPIO-output service ships disabled (the unit guard execs /bin/true
        // until the operator opts in), so a missing binary degrades nothing and
        // must never abort the install.
        assert_eq!(gpio.gate, Gate::BestEffort);
        assert_eq!(gpio.release_tag, "prebuilt-gpio");
        assert!(for_profile("drone")
            .iter()
            .any(|b| b.service == "ados-gpio"));
        assert!(for_profile("ground_station")
            .iter()
            .any(|b| b.service == "ados-gpio"));
    }

    #[test]
    fn exactly_five_hard_and_they_are_the_right_ones() {
        let hard: Vec<&str> = PREBUILT
            .iter()
            .filter(|b| b.gate == Gate::Hard)
            .map(|b| b.service)
            .collect();
        assert_eq!(hard.len(), 5, "hard gates: {hard:?}");
        for svc in [
            "ados-supervisor",
            "ados-mavlink-router",
            "ados-video",
            "ados-cloud",
            "ados-vision",
        ] {
            assert!(hard.contains(&svc), "{svc} must be a Hard gate");
        }
    }

    #[test]
    fn mavlink_router_is_hard_on_both_profiles() {
        // The router is the sole C2 path with no Python fallback; its absence
        // must fail the install on either profile, so it is a Hard gate that
        // ships on both.
        let router = PREBUILT
            .iter()
            .find(|b| b.service == "ados-mavlink-router")
            .expect("ados-mavlink-router must be in the catalog");
        assert_eq!(router.gate, Gate::Hard);
        assert!(for_profile("drone")
            .iter()
            .any(|b| b.service == "ados-mavlink-router"));
        assert!(for_profile("ground_station")
            .iter()
            .any(|b| b.service == "ados-mavlink-router"));
    }

    #[test]
    fn logd_is_best_effort_on_both_profiles() {
        let logd = PREBUILT
            .iter()
            .find(|b| b.service == "ados-logd")
            .expect("ados-logd must be in the catalog");
        // A missing store degrades recordkeeping; it must never abort a fresh
        // install, so its gate is best-effort.
        assert_eq!(logd.gate, Gate::BestEffort);
        // The store captures from both the drone and ground-station service
        // sets, so it ships on both profiles.
        assert!(for_profile("drone")
            .iter()
            .any(|b| b.service == "ados-logd"));
        assert!(for_profile("ground_station")
            .iter()
            .any(|b| b.service == "ados-logd"));
    }

    #[test]
    fn control_is_best_effort_on_both_profiles() {
        let control = PREBUILT
            .iter()
            .find(|b| b.service == "ados-control")
            .expect("ados-control must be in the catalog");
        // The control surface ships disabled (the GCS uses the FastAPI surface),
        // so a missing binary degrades nothing and must never abort the install.
        assert_eq!(control.gate, Gate::BestEffort);
        assert_eq!(control.release_tag, "prebuilt-control");
        // Cross-profile: it serves both the drone and ground-station agents.
        assert!(for_profile("drone")
            .iter()
            .any(|b| b.service == "ados-control"));
        assert!(for_profile("ground_station")
            .iter()
            .any(|b| b.service == "ados-control"));
    }

    #[test]
    fn asset_matches_service_aarch64() {
        for b in PREBUILT {
            assert_eq!(b.asset, format!("{}-aarch64", b.service).as_str());
        }
    }

    #[test]
    fn ados_service_dest_is_under_bin_dir() {
        for b in PREBUILT.iter().filter(|b| b.service.starts_with("ados-")) {
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
