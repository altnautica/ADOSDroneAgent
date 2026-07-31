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
        service: "ados-oled-i2c",
        asset: "ados-oled-i2c-aarch64",
        release_tag: "prebuilt-display",
        dest: "/opt/ados/bin/ados-oled-i2c",
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
    // The world-model capture service. Best-effort + opt-in: it runs behind the
    // capture feature flag (inert by default), so a missing binary degrades only
    // the opt-in capture path without aborting the install. Fetched + placed so
    // enabling capture works on demand — and, crucially, so an upgrade keeps it in
    // step with the vision engine it shares a shared-memory ring layout with.
    PrebuiltBinary {
        service: "ados-atlas",
        asset: "ados-atlas-aarch64",
        release_tag: "prebuilt-atlas",
        dest: "/opt/ados/bin/ados-atlas",
        gate: Gate::BestEffort,
        profiles: DRONE,
    },
    // The decentralized swarm state bus. Fetched on BOTH FC-bearing profiles,
    // matching its supervisor gate: a drone broadcasts its beacon and a ground
    // station listens so the operator's fleet view is local-first. Best-effort
    // rather than Hard: the unit `ConditionPathExists`-gates on the binary, so a
    // fetch miss leaves single-drone operation entirely intact — the bus adds fleet
    // awareness and onboard separation input, and neither is on the C2 path.
    //
    // A fetch miss is NOT caught anywhere, which this comment previously claimed
    // it was: the health gate checks Hard-gated binaries plus three named
    // exceptions, and the swarm bus is none of them. That claim mattered,
    // because for a long time no publish job existed for this release at all, so
    // every install on both profiles 404'd here and continued degraded in
    // silence — the unit's ConditionPathExists skipped it, the socket was never
    // bound, and the swarm control loop suppressed every tick as stale
    // neighbours. Fleet awareness and onboard separation were simply absent.
    // If this is ever demoted or the publish job is removed, the absence has to
    // surface somewhere an operator will see it.
    PrebuiltBinary {
        service: "ados-swarmbus",
        asset: "ados-swarmbus-aarch64",
        release_tag: "prebuilt-swarmbus",
        dest: "/opt/ados/bin/ados-swarmbus",
        gate: Gate::BestEffort,
        profiles: BOTH,
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
    // The CRSF / ExpressLRS RC control lane. Best-effort and opt-in: the unit
    // gates on the crsf-enabled marker (mirroring radio.crsf.enabled) and its
    // ExecStart guard execs /bin/true until the binary lands, so a missing
    // asset degrades nothing. Fetched on both profiles: the ground node drives
    // the RC transmitter module today, the drone side carries the relay
    // last-mile posture.
    PrebuiltBinary {
        service: "ados-crsf",
        asset: "ados-crsf-aarch64",
        release_tag: "prebuilt-crsf",
        dest: "/opt/ados/bin/ados-crsf",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
    // The config-over-radio channel service. ExecStart guard execs /bin/true
    // until the binary lands, so a missing asset degrades nothing. Fetched on
    // both profiles: the drone runs the terminator, the ground node the
    // injector.
    PrebuiltBinary {
        service: "ados-tunnel-config",
        asset: "ados-tunnel-config-aarch64",
        release_tag: "prebuilt-tunnel-config",
        dest: "/opt/ados/bin/ados-tunnel-config",
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
    // The compute reconstructor/offload daemon. Best-effort so a workstation
    // host that cannot use this aarch64 prebuilt degrades + reports rather
    // than failing the install. How a workstation gets the daemon by host:
    //   - macOS (any arch): the macOS install path builds every service from
    //     source, so this catalog entry is not consulted there.
    //   - aarch64 Linux: this prebuilt is fetched.
    //   - non-aarch64 Linux (e.g. an x86_64 GPU box): NOT yet supported — the
    //     preflight arch gate stops the Linux install before any fetch, and a
    //     Linux build-from-source path is a scoped follow-up.
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

/// The ONNX-enabled `ados-vision` variant, fetched for a board that declares
/// CPU-ONNX local inference (an NPU-less but CPU-strong board — see
/// [`board_prefers_onnx_vision`]). Same install destination as the default vision
/// binary (`/opt/ados/bin/ados-vision`): it is the SAME service, built with the
/// onnx feature so it runs the detector on the CPU. A separate release tag +
/// asset so the default build is untouched — the default ships as a static musl
/// binary, which cannot link ONNX Runtime (no musl prebuilt), so the onnx build
/// is a distinct glibc asset published by its own release job. Not part of the
/// `PREBUILT` catalog: the fetch step selects it in place of the default vision
/// entry, with the default as a fallback so a missing onnx asset never aborts an
/// install.
pub const PREBUILT_VISION_ONNX: PrebuiltBinary = PrebuiltBinary {
    service: "ados-vision",
    asset: "ados-vision-onnx-aarch64",
    release_tag: "prebuilt-vision-onnx",
    dest: "/opt/ados/bin/ados-vision",
    gate: Gate::Hard,
    profiles: DRONE,
};

/// The ONNX Runtime shared library the onnx-enabled `ados-vision` binary loads at
/// start (via `ORT_DYLIB_PATH`). Published under the same tag as the onnx binary
/// (`prebuilt-vision-onnx`); an official aarch64 build whose glibc floor is well
/// below the target board's, so it runs where the pyke build the crate would
/// otherwise download does not. The onnx binary links the runtime dynamically, so
/// this library must be present for it to run — the fetch step installs the two
/// together and falls back to the default (musl, no-onnx) vision build if either
/// is unavailable. Placed alongside `ados-vision`'s config, not on `bin/`.
pub const PREBUILT_VISION_ONNX_RUNTIME: PrebuiltBinary = PrebuiltBinary {
    service: "libonnxruntime",
    asset: "libonnxruntime-aarch64.so",
    release_tag: "prebuilt-vision-onnx",
    dest: "/opt/ados/lib/libonnxruntime.so",
    gate: Gate::BestEffort,
    profiles: DRONE,
};

/// Board-model substrings that get the ONNX-enabled `ados-vision` build. Matched
/// case-insensitively against the device-tree model string, mirroring the board
/// profiles that declare `compute.local_inference: onnx` (Cortex-A76-class,
/// NPU-less boards a CPU YOLO runs usefully on). Keep this list in step with
/// those YAML profiles. NPU-class boards are normally excluded — they run the
/// accelerator sidecar, not the CPU ONNX build — but `sun60iw2` (Allwinner A733,
/// Radxa Cubie A7S) is a deliberate exception: its VIP9000 NPU has no in-tree
/// backend (no TIM-VX support yet — see `cubie-a7s.yaml`'s Rule-44 note), so it
/// runs the CPU ONNX build like an NPU-less board until that backend lands.
const ONNX_VISION_BOARD_SUBSTRINGS: &[&str] = &[
    "raspberry pi 5",
    "compute module 5",
    "cm5",
    "sun60iw2",
    "cubie a7s",
    "a733",
];

/// Whether the board model declares CPU-ONNX local inference and should fetch the
/// onnx-enabled vision build. Pure, case-insensitive substring match.
pub fn board_prefers_onnx_vision(model: &str) -> bool {
    let m = model.to_lowercase();
    ONNX_VISION_BOARD_SUBSTRINGS.iter().any(|k| m.contains(k))
}

/// The default `ados-vision` catalog entry (the static musl build, no onnx).
pub fn default_vision_binary() -> &'static PrebuiltBinary {
    PREBUILT
        .iter()
        .find(|b| b.service == "ados-vision")
        .expect("ados-vision is in the catalog")
}

/// The `ados-vision` prebuilt to fetch for a board: the ONNX-enabled build when
/// the board declares CPU-ONNX local inference, else the default build. The fetch
/// step falls back to the default when the onnx variant cannot be fetched.
pub fn vision_binary(model: &str) -> &'static PrebuiltBinary {
    if board_prefers_onnx_vision(model) {
        &PREBUILT_VISION_ONNX
    } else {
        default_vision_binary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// The workflow that builds and publishes every prebuilt this catalog fetches.
    /// Located relative to the crate rather than the process cwd so the test holds
    /// under `cargo test` from anywhere in the workspace.
    ///
    /// Scoped to this one workflow deliberately: the RTL8812EU kernel-module
    /// prebuilts live in `driver-build.yml` under `prebuilt-drivers` and are fetched
    /// by the shell driver layer, not by this catalog, so they are not in scope here.
    fn rust_workflow_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/workflows/rust.yml")
            .canonicalize()
            .expect("the Rust workflow must exist beside the crate that depends on it")
    }

    /// Every `tag_name:` the Rust workflow publishes to, mapped to the job that
    /// publishes it. Parsed from the workflow itself — a second hand-maintained copy
    /// of the tag list would drift exactly the way the catalog already did.
    fn published_release_tags() -> BTreeMap<String, String> {
        let path = rust_workflow_path();
        let text = std::fs::read_to_string(&path).expect("the Rust workflow must be readable");
        let root: serde_norway::Value =
            serde_norway::from_str(&text).expect("the Rust workflow must be valid YAML");
        let jobs = root
            .get("jobs")
            .and_then(|j| j.as_mapping())
            .expect("the Rust workflow must declare jobs");

        let mut found = BTreeMap::new();
        for (job_name, job) in jobs {
            let job_name = job_name.as_str().unwrap_or("<unnamed job>").to_string();

            // A job that publishes with the `gh` CLI rather than the release
            // action has no `with.tag_name` to read, so it declares the tag as
            // a `RELEASE_TAG` job variable instead. Recognised here so such a
            // job is still tied to the catalog; the alternative is matching the
            // tag inside a shell line, which would tie this guard to the exact
            // spelling of a script.
            if let Some(tag) = job.get("env").and_then(|e| e.get("RELEASE_TAG")) {
                let tag = tag
                    .as_str()
                    .unwrap_or_else(|| panic!("job `{job_name}` has a non-string RELEASE_TAG"));
                assert!(
                    !tag.contains("${{"),
                    "job `{job_name}` publishes to a templated tag `{tag}`; this test can \
                     only tie literal tags to the catalog, so either pin the tag or extend \
                     this check to resolve it"
                );
                found.entry(tag.to_string()).or_insert(job_name.clone());
            }

            let Some(steps) = job.get("steps").and_then(|s| s.as_sequence()) else {
                continue;
            };
            for step in steps {
                let Some(tag) = step.get("with").and_then(|w| w.get("tag_name")) else {
                    continue;
                };
                let tag = tag
                    .as_str()
                    .unwrap_or_else(|| panic!("job `{job_name}` has a non-string tag_name"));
                // A templated tag (`${{ ... }}`) cannot be resolved at test time, so it
                // would silently drop out of this check and reopen the exact hole the
                // test closes. Fail loudly instead of skipping.
                assert!(
                    !tag.contains("${{"),
                    "job `{job_name}` publishes to a templated tag `{tag}`; this test can \
                     only tie literal tags to the catalog, so either pin the tag or extend \
                     this check to resolve it"
                );
                found.insert(tag.to_string(), job_name.clone());
            }
        }
        assert!(
            !found.is_empty(),
            "parsed no publish tags out of {} — the workflow shape changed and this \
             check silently stopped guarding anything",
            path.display()
        );
        found
    }

    /// Every release tag an install will actually fetch from: the catalog, plus the
    /// two out-of-catalog entries the fetch step selects explicitly (the onnx vision
    /// build and the ONNX Runtime library that ships with it). Both are fetched on a
    /// real install, so both carry the same exposure as a catalog row.
    fn fetched_release_tags() -> BTreeMap<&'static str, &'static str> {
        let mut tags = BTreeMap::new();
        for b in PREBUILT
            .iter()
            .chain([&PREBUILT_VISION_ONNX, &PREBUILT_VISION_ONNX_RUNTIME])
        {
            tags.entry(b.release_tag).or_insert(b.service);
        }
        tags
    }

    /// Tags the workflow publishes that no install-time fetch claims — each one
    /// named, with the reason it is legitimately absent from the catalog. An
    /// unexplained entry here is a publish job nobody consumes.
    const PUBLISHED_BUT_NOT_FETCHED: &[(&str, &str)] = &[
        // The installer binary itself. `scripts/install.sh` downloads it from this
        // tag to bootstrap the install, which is what then reads this catalog — so
        // it cannot appear in the catalog it is a precondition for.
        (
            "prebuilt-installer",
            "bootstrapped directly by scripts/install.sh before the catalog is read",
        ),
    ];

    /// The catalog names a release tag for every binary an install fetches, but
    /// nothing tied those names to the workflow that creates the releases. When the
    /// swarm bus was added to the catalog with no matching publish job, every install
    /// on both profiles 404'd the asset and continued degraded in silence: the unit's
    /// `ConditionPathExists` skipped it, the socket was never bound, and fleet
    /// awareness plus onboard separation input were simply absent from the field.
    /// A typo'd or renamed tag fails exactly the same way — quietly, at install time,
    /// on hardware, with a green CI run behind it.
    #[test]
    fn every_release_tag_the_installer_fetches_is_published_by_the_rust_workflow() {
        let published = published_release_tags();
        let missing: Vec<String> = fetched_release_tags()
            .iter()
            .filter(|(tag, _)| !published.contains_key(**tag))
            .map(|(tag, service)| format!("`{tag}` (fetched for {service})"))
            .collect();

        assert!(
            missing.is_empty(),
            "no job in .github/workflows/rust.yml publishes these release tags, so every \
             install will 404 the asset and continue degraded in silence: {}",
            missing.join(", ")
        );
    }

    /// The mirror failure, and just as quiet: a job that builds and publishes an
    /// asset no install ever fetches. Nothing breaks loudly — CI stays green and a
    /// release keeps filling up — but the binary reaches no rig, so a service
    /// believed to be shipping is not installed anywhere. Renaming a tag on one side
    /// only produces both halves of this at once.
    #[test]
    fn every_release_tag_the_rust_workflow_publishes_is_fetched_by_the_installer() {
        let fetched = fetched_release_tags();
        let orphans: Vec<String> = published_release_tags()
            .iter()
            .filter(|(tag, _)| !fetched.contains_key(tag.as_str()))
            .filter(|(tag, _)| {
                !PUBLISHED_BUT_NOT_FETCHED
                    .iter()
                    .any(|(exempt, _)| exempt == tag)
            })
            .map(|(tag, job)| format!("`{tag}` (published by job `{job}`)"))
            .collect();

        assert!(
            orphans.is_empty(),
            "these release tags are published but nothing fetches them, so the binaries \
             reach no rig: {}. If an entry is deliberate, add it to \
             PUBLISHED_BUT_NOT_FETCHED with the reason.",
            orphans.join(", ")
        );
    }

    /// A stale exemption is the same silence wearing a permission slip: it would keep
    /// excusing a tag that no longer exists while reading as a considered decision.
    #[test]
    fn every_named_publish_exemption_still_names_a_real_tag() {
        let published = published_release_tags();
        for (tag, reason) in PUBLISHED_BUT_NOT_FETCHED {
            assert!(
                published.contains_key(*tag),
                "`{tag}` is exempted from the catalog ({reason}) but no job in \
                 .github/workflows/rust.yml publishes it any more — drop the exemption"
            );
        }
    }

    #[test]
    fn catalog_has_twenty_four_entries() {
        assert_eq!(PREBUILT.len(), 24);
    }

    /// Both FC-bearing profiles fetch the swarm bus, matching its supervisor gate. A
    /// ground station that fetched only the drone half would have no fleet view of its
    /// own and would fall back to a cloud round-trip for something every node already
    /// hears on the air.
    #[test]
    fn both_fc_profiles_fetch_the_swarm_bus() {
        for profile in ["drone", "ground_station"] {
            let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
            assert!(
                svcs.contains(&"ados-swarmbus"),
                "{profile} must fetch ados-swarmbus"
            );
        }
        // Best-effort, not Hard: a fetch miss must leave single-drone operation intact
        // rather than abort the install. The bus is not on the C2 path.
        let bus = PREBUILT
            .iter()
            .find(|b| b.service == "ados-swarmbus")
            .expect("ados-swarmbus must be in the catalog");
        assert_eq!(bus.gate, Gate::BestEffort);
        // The path the unit's ConditionPathExists + ExecStart name, or the unit
        // silently never starts.
        assert_eq!(bus.dest, "/opt/ados/bin/ados-swarmbus");
        assert_eq!(bus.asset, "ados-swarmbus-aarch64");
    }

    #[test]
    fn both_profiles_fetch_the_config_tunnel_binary() {
        for profile in ["drone", "ground_station"] {
            let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
            assert!(
                svcs.contains(&"ados-tunnel-config"),
                "{profile} must fetch ados-tunnel-config for the config-over-radio channel"
            );
        }
    }

    #[test]
    fn drone_profile_fetches_the_atlas_capture_service() {
        let svcs: Vec<&str> = for_profile("drone").iter().map(|b| b.service).collect();
        assert!(
            svcs.contains(&"ados-atlas"),
            "a drone install must fetch ados-atlas so an upgrade keeps the ring \
             reader in step with the vision engine that writes the ring"
        );
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
        assert_eq!(tag("ados-oled-i2c"), "prebuilt-display");
    }

    #[test]
    fn onnx_vision_variant_targets_the_same_service_and_destination() {
        // The onnx build is the SAME service installed at the SAME path as the
        // default vision binary — only the fetched asset differs.
        let default = default_vision_binary();
        assert_eq!(PREBUILT_VISION_ONNX.service, default.service);
        assert_eq!(PREBUILT_VISION_ONNX.dest, default.dest);
        assert_eq!(PREBUILT_VISION_ONNX.dest, "/opt/ados/bin/ados-vision");
        // A distinct asset + release tag so the default (musl) build is untouched.
        assert_ne!(PREBUILT_VISION_ONNX.asset, default.asset);
        assert_ne!(PREBUILT_VISION_ONNX.release_tag, default.release_tag);
        assert_eq!(PREBUILT_VISION_ONNX.asset, "ados-vision-onnx-aarch64");
        assert_eq!(PREBUILT_VISION_ONNX.release_tag, "prebuilt-vision-onnx");
        // Not part of the catalog (it is selected in place of the default entry).
        assert!(!PREBUILT
            .iter()
            .any(|b| b.asset == PREBUILT_VISION_ONNX.asset));
    }

    #[test]
    fn onnx_runtime_library_ships_beside_the_onnx_binary() {
        // The ONNX Runtime shared library rides the same release tag as the onnx
        // binary, installs off `bin/`, and is best-effort so a miss falls back to
        // the default vision build rather than aborting the install.
        assert_eq!(
            PREBUILT_VISION_ONNX_RUNTIME.release_tag,
            PREBUILT_VISION_ONNX.release_tag
        );
        assert_eq!(
            PREBUILT_VISION_ONNX_RUNTIME.asset,
            "libonnxruntime-aarch64.so"
        );
        assert_eq!(
            PREBUILT_VISION_ONNX_RUNTIME.dest,
            "/opt/ados/lib/libonnxruntime.so"
        );
        assert!(matches!(
            PREBUILT_VISION_ONNX_RUNTIME.gate,
            Gate::BestEffort
        ));
        // Not part of the catalog (fetched alongside the onnx binary explicitly).
        assert!(!PREBUILT
            .iter()
            .any(|b| b.asset == PREBUILT_VISION_ONNX_RUNTIME.asset));
    }

    #[test]
    fn onnx_vision_selected_only_for_cpu_strong_boards() {
        // CPU-strong, NPU-less boards that declare local ONNX inference.
        assert!(board_prefers_onnx_vision("Raspberry Pi 5 Model B Rev 1.0"));
        assert!(board_prefers_onnx_vision("Raspberry Pi Compute Module 5"));
        assert!(board_prefers_onnx_vision("Raspberry Pi CM5"));
        // A733/Cubie A7S: NPU present but no in-tree backend yet, so it is a
        // deliberate exception to the "NPU boards run the sidecar" rule below.
        assert!(board_prefers_onnx_vision("sun60iw2"));
        assert!(board_prefers_onnx_vision("Radxa Cubie A7S"));
        // NPU boards WITH an in-tree backend run the sidecar, not the CPU ONNX build.
        assert!(!board_prefers_onnx_vision("Radxa ROCK 5C Lite (RK3582)"));
        // Weaker / unknown boards stay on the default build.
        assert!(!board_prefers_onnx_vision("Raspberry Pi 4 Model B"));
        assert!(!board_prefers_onnx_vision(""));
    }

    #[test]
    fn vision_binary_resolves_the_variant_by_board() {
        // A CPU-strong board resolves to the onnx build; everything else to the
        // default catalog entry.
        assert_eq!(
            vision_binary("Raspberry Pi 5 Model B").asset,
            "ados-vision-onnx-aarch64"
        );
        assert_eq!(
            vision_binary("Radxa ROCK 5C Lite").asset,
            default_vision_binary().asset
        );
        assert_eq!(vision_binary("").asset, default_vision_binary().asset);
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
