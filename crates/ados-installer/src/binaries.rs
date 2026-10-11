//! The prebuilt-binary catalog.
//!
//! Each service ships as an `<asset>` in the per-commit `rev-<sha>` release
//! CI builds in one job. The fetch step resolves which release to read (the
//! `prebuilt-main` manifest on edge, `--ref`, or the stable version release),
//! downloads the assets for the active profile from it, verifies them, and
//! drops each at its destination (the `ados-*` services under
//! `/opt/ados/bin/<service>`; a mirrored third-party relay under the system bin
//! dir). A `Hard` gate means a missing/failed binary fails the install; a
//! `BestEffort` gate degrades it.

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
    /// Install destination.
    pub dest: &'static str,
    /// Hard vs best-effort gate.
    pub gate: Gate,
    /// Profiles that need this binary (`drone`, `ground_station`,
    /// `workstation`, `compute`).
    pub profiles: &'static [&'static str],
}

/// Profile constants — kept as slices so the const table can reference them.
const BOTH: &[&str] = &["drone", "ground_station"];
const DRONE: &[&str] = &["drone"];
const GROUND: &[&str] = &["ground_station"];
/// Every profile, including the workstation-class nodes. Used for the
/// profile-agnostic core services every node needs (orchestrator, cloud relay,
/// control front, plugin host, logging, TUI) so a `--profile workstation` or
/// `--profile compute` install fetches them too.
const ANY: &[&str] = &["drone", "ground_station", "workstation", "compute"];

/// The full catalog of prebuilt service binaries.
///
/// Gate rationale: the agent cannot do its job without the orchestrator
/// (`ados-supervisor`), the MAVLink router (`ados-mavlink-router`), the video
/// pipeline (`ados-video`), the cloud-relay transport (`ados-cloud`), the
/// vision host (`ados-vision`), or the LAN front (`ados-control`), so those are
/// `Hard`. The router is the sole command-and-control path to the flight
/// controller — the packaged Python MAVLink service it replaced is gone, so a
/// missing router leaves the Core MAVLink unit crash-looping with no FC
/// telemetry, arming, or GCS link. A fetch miss must therefore FAIL the install
/// rather than report it healthy. Everything else degrades to best-effort: the
/// agent still comes up and reports the missing capability via the install
/// result.
pub const PREBUILT: &[PrebuiltBinary] = &[
    PrebuiltBinary {
        service: "ados-tui",
        asset: "ados-tui-aarch64",
        dest: "/opt/ados/bin/ados-tui",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-supervisor",
        asset: "ados-supervisor-aarch64",
        dest: "/opt/ados/bin/ados-supervisor",
        gate: Gate::Hard,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-mavlink-router",
        asset: "ados-mavlink-router-aarch64",
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
        dest: "/opt/ados/bin/ados-radio",
        gate: Gate::BestEffort,
        profiles: DRONE,
    },
    PrebuiltBinary {
        service: "ados-video",
        asset: "ados-video-aarch64",
        dest: "/opt/ados/bin/ados-video",
        gate: Gate::Hard,
        profiles: DRONE,
    },
    PrebuiltBinary {
        service: "ados-camera-probe",
        asset: "ados-camera-probe-aarch64",
        // Built in the same job as ados-video, so the probe can never be a
        // version behind the pipeline whose overlay it validates.
        dest: "/opt/ados/bin/ados-camera-probe",
        // Best-effort, and fetched on every profile even though the CSI camera
        // is drone-side: the unit is `ConditionPathExists`-gated on the
        // probation marker, so a node that never staged a camera overlay never
        // execs it, and any node that DID stage one must be able to revert it.
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    // The native plugin host: the per-plugin sockets, token minting and unit
    // refresh. Every profile runs it — extensions target workstation and compute
    // nodes as well as the SBC profiles — so it is fetched on all of them.
    PrebuiltBinary {
        service: "ados-plugin-host",
        asset: "ados-plugin-host-aarch64",
        dest: "/opt/ados/bin/ados-plugin-host",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-cloud",
        asset: "ados-cloud-aarch64",
        dest: "/opt/ados/bin/ados-cloud",
        gate: Gate::Hard,
        profiles: ANY,
    },
    PrebuiltBinary {
        service: "ados-groundlink",
        asset: "ados-groundlink-aarch64",
        dest: "/opt/ados/bin/ados-groundlink",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-net",
        asset: "ados-net-aarch64",
        dest: "/opt/ados/bin/ados-net",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-pic",
        asset: "ados-pic-aarch64",
        dest: "/opt/ados/bin/ados-pic",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-input",
        asset: "ados-input-aarch64",
        dest: "/opt/ados/bin/ados-input",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-display",
        asset: "ados-display-aarch64",
        dest: "/opt/ados/bin/ados-display",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-display-probe",
        asset: "ados-display-probe-aarch64",
        dest: "/opt/ados/bin/ados-display-probe",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-oled-i2c",
        asset: "ados-oled-i2c-aarch64",
        dest: "/opt/ados/bin/ados-oled-i2c",
        gate: Gate::BestEffort,
        profiles: GROUND,
    },
    PrebuiltBinary {
        service: "ados-vision",
        asset: "ados-vision-aarch64",
        dest: "/opt/ados/bin/ados-vision",
        gate: Gate::Hard,
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
    // it was: the health gate checks Hard-gated binaries plus the named WFB and
    // display exceptions, and the swarm bus is none of them. That claim mattered,
    // because for a long time nothing published this binary at all, so
    // every install on both profiles 404'd here and continued degraded in
    // silence — the unit's ConditionPathExists skipped it, the socket was never
    // bound, and the swarm control loop suppressed every tick as stale
    // neighbours. Fleet awareness and onboard separation were simply absent.
    // If this is ever demoted or the publish job is removed, the absence has to
    // surface somewhere an operator will see it.
    PrebuiltBinary {
        service: "ados-swarmbus",
        asset: "ados-swarmbus-aarch64",
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
        dest: "/opt/ados/bin/ados-logd",
        gate: Gate::BestEffort,
        profiles: ANY,
    },
    // The native HTTP control surface: the LAN front on `:8080` (pairing, status,
    // every migrated REST route), on every profile. The installer writes the
    // front marker on every install and the front drop-in moves the residual
    // Python API onto an internal socket behind it, so without this binary
    // nothing answers on `:8080` and the node cannot be paired or reached. Hard:
    // a fetch miss must fail here, naming the binary, not surface later as an
    // unreachable REST port.
    PrebuiltBinary {
        service: "ados-control",
        asset: "ados-control-aarch64",
        dest: "/opt/ados/bin/ados-control",
        gate: Gate::Hard,
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
        dest: "/usr/local/bin/mediamtx",
        gate: Gate::BestEffort,
        profiles: BOTH,
    },
];

/// The subset of the catalog needed by `profile`
/// (`drone` | `ground_station` | `workstation` | `compute`).
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
/// onnx feature so it runs the detector on the CPU. A separate asset so the
/// default build is untouched — the default ships as a static musl binary,
/// which cannot link ONNX Runtime (no musl prebuilt), so the onnx build is a
/// distinct glibc asset built by its own job. Not part of the
/// `PREBUILT` catalog: the fetch step selects it in place of the default vision
/// entry, with the default as a fallback so a missing onnx asset never aborts an
/// install.
pub const PREBUILT_VISION_ONNX: PrebuiltBinary = PrebuiltBinary {
    service: "ados-vision",
    asset: "ados-vision-onnx-aarch64",
    dest: "/opt/ados/bin/ados-vision",
    gate: Gate::Hard,
    profiles: DRONE,
};

/// The ONNX Runtime shared library the onnx-enabled `ados-vision` binary loads at
/// start (via `ORT_DYLIB_PATH`). Published in the same release as the onnx
/// binary; an official aarch64 build whose glibc floor is well
/// below the target board's, so it runs where the pyke build the crate would
/// otherwise download does not. The onnx binary links the runtime dynamically, so
/// this library must be present for it to run — the fetch step installs the two
/// together and falls back to the default (musl, no-onnx) vision build if either
/// is unavailable. Placed alongside `ados-vision`'s config, not on `bin/`.
pub const PREBUILT_VISION_ONNX_RUNTIME: PrebuiltBinary = PrebuiltBinary {
    service: "libonnxruntime",
    asset: "libonnxruntime-aarch64.so",
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
///
/// The board string can also be the operator's `/etc/ados/board_override`,
/// whose grammar is the board-profile YAML filename STEM — so every onnx board's
/// stem (`cm5`, `rpi5`, `cubie-a7s`) is listed alongside its display form. A
/// stem that resolved to no entry here silently downgraded a pinned board to the
/// non-ONNX build.
const ONNX_VISION_BOARD_SUBSTRINGS: &[&str] = &[
    "raspberry pi 5",
    "rpi5",
    "compute module 5",
    "cm5",
    "sun60iw2",
    "cubie a7s",
    "cubie-a7s",
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
    use std::collections::BTreeSet;
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

    /// The parsed workflow.
    fn rust_workflow_root() -> serde_norway::Value {
        let path = rust_workflow_path();
        let text = std::fs::read_to_string(&path).expect("the Rust workflow must be readable");
        serde_norway::from_str(&text).expect("the Rust workflow must be valid YAML")
    }

    /// The job that publishes `rev-<sha>` and moves the `prebuilt-main` pointer.
    const PUBLISH_JOB: &str = "publish";

    /// The publish job's declaration.
    fn publish_job() -> serde_norway::Value {
        rust_workflow_root()
            .get("jobs")
            .and_then(|jobs| jobs.get(PUBLISH_JOB))
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "no `{PUBLISH_JOB}` job in .github/workflows/rust.yml — every install \
                     resolves its binaries from the release that job publishes"
                )
            })
    }

    /// Every asset a `rev-<sha>` release carries, read from the workflow's
    /// `REV_ASSETS` variable (the one list the build, the publish check and the
    /// manifest all derive from).
    fn rev_assets() -> BTreeSet<String> {
        let root = rust_workflow_root();
        let raw = root
            .get("env")
            .and_then(|e| e.get("REV_ASSETS"))
            .and_then(|v| v.as_str())
            .expect("rust.yml must declare its release asset list as a top-level REV_ASSETS")
            .to_string();
        assert!(
            !raw.contains("${{"),
            "REV_ASSETS is templated (`{raw}`); only a literal list can be tied to the catalog"
        );
        raw.split_whitespace().map(str::to_string).collect()
    }

    /// Every asset an install fetches: the catalog, plus the two out-of-catalog
    /// entries the fetch step selects explicitly (the onnx vision build and the
    /// ONNX Runtime library that ships with it).
    fn fetched_assets() -> BTreeSet<String> {
        PREBUILT
            .iter()
            .chain([&PREBUILT_VISION_ONNX, &PREBUILT_VISION_ONNX_RUNTIME])
            .map(|b| b.asset.to_string())
            .collect()
    }

    /// The catalog and the release must name the same assets, in both directions.
    /// An asset fetched but never published 404s at install time and the service
    /// continues degraded in silence (the swarm bus shipped that way for a long
    /// time); an asset published but never fetched reaches no rig.
    #[test]
    fn the_release_carries_exactly_the_assets_the_installer_fetches() {
        let published = rev_assets();
        let fetched = fetched_assets();
        let missing: Vec<&String> = fetched.difference(&published).collect();
        assert!(
            missing.is_empty(),
            "rust.yml REV_ASSETS does not publish these fetched assets, so every install \
             404s them: {missing:?}"
        );
        let orphans: Vec<&String> = published.difference(&fetched).collect();
        assert!(
            orphans.is_empty(),
            "rust.yml REV_ASSETS publishes assets nothing fetches: {orphans:?}"
        );
    }

    /// The publish job waits for both build jobs, publishes the per-commit
    /// release, moves the edge pointer, and keeps the installer bootstrap
    /// (`scripts/install.sh` downloads it from `prebuilt-installer` before any
    /// manifest can be read) current.
    #[test]
    fn the_publish_job_builds_one_release_and_moves_one_pointer() {
        let job = publish_job();
        let needs: BTreeSet<String> = job
            .get("needs")
            .and_then(|n| n.as_sequence())
            .expect("the publish job must declare `needs`")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for build in ["prebuilt", "vision-onnx"] {
            assert!(
                needs.contains(build),
                "`{PUBLISH_JOB}` must wait for `{build}`, or it publishes a partial release"
            );
        }
        let script = serde_norway::to_string(&job).expect("the job must re-serialize");
        for needle in [
            "rev-${GITHUB_SHA}",
            "prebuilt-main",
            "manifest.json",
            "prebuilt-installer",
        ] {
            assert!(
                script.contains(needle),
                "`{PUBLISH_JOB}` no longer mentions `{needle}`"
            );
        }
        // No per-service rolling tag survives: an install reads only the pointer
        // and the release it names.
        let text = std::fs::read_to_string(rust_workflow_path()).unwrap();
        assert!(
            !text.contains("RELEASE_TAG"),
            "rust.yml declares a per-service RELEASE_TAG again"
        );
    }

    #[test]
    fn catalog_has_twenty_three_entries() {
        assert_eq!(PREBUILT.len(), 23);
    }

    /// The camera boot probe is fetched on every profile even though the CSI
    /// camera is drone-side. Its unit is gated on the probation marker, so a
    /// node that never staged an overlay never execs it — and any node that DID
    /// stage one must carry the binary that can revert it, or a blind apply on a
    /// board with no camera has nothing to undo it.
    #[test]
    fn every_profile_fetches_the_camera_boot_probe() {
        for profile in ["drone", "ground_station", "workstation", "compute"] {
            let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
            assert!(
                svcs.contains(&"ados-camera-probe"),
                "{profile} must fetch ados-camera-probe"
            );
        }
        let probe = PREBUILT
            .iter()
            .find(|b| b.service == "ados-camera-probe")
            .expect("the camera probe must be in the catalog");
        assert_eq!(probe.dest, "/opt/ados/bin/ados-camera-probe");
        // Best-effort: a fetch miss must not abort an install on a board with no
        // camera at all.
        assert_eq!(probe.gate, Gate::BestEffort);
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

    /// The workstation-class profiles are full agents. `compute` used to be absent
    /// from every profile set, so a `--profile compute` Linux install fetched no
    /// binary at all — not even the supervisor its unit execs.
    #[test]
    fn workstation_class_profiles_fetch_the_cores() {
        for profile in ["workstation", "compute"] {
            let svcs: Vec<&str> = for_profile(profile).iter().map(|b| b.service).collect();
            // The orchestrator, cloud relay, control front (LAN pairing), plugin
            // host, logging, and TUI.
            for svc in [
                "ados-supervisor",
                "ados-cloud",
                "ados-control",
                "ados-plugin-host",
                "ados-logd",
                "ados-tui",
            ] {
                assert!(
                    svcs.contains(&svc),
                    "{profile} profile must fetch {svc}: {svcs:?}"
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
                    "{profile} profile must NOT fetch {svc}: {svcs:?}"
                );
            }
        }
    }

    /// Extensions target every node profile, so every profile runs the plugin
    /// host. The shipped unit is enable-linked everywhere; a profile that did not
    /// fetch the binary left it inactive behind its `ConditionPathExists`.
    #[test]
    fn every_profile_fetches_the_plugin_host() {
        for profile in ["drone", "ground_station", "workstation", "compute"] {
            assert!(
                for_profile(profile)
                    .iter()
                    .any(|b| b.service == "ados-plugin-host"),
                "{profile} must fetch ados-plugin-host"
            );
        }
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
        assert!(for_profile("drone")
            .iter()
            .any(|b| b.service == "ados-gpio"));
        assert!(for_profile("ground_station")
            .iter()
            .any(|b| b.service == "ados-gpio"));
    }

    #[test]
    fn exactly_six_hard_and_they_are_the_right_ones() {
        let hard: Vec<&str> = PREBUILT
            .iter()
            .filter(|b| b.gate == Gate::Hard)
            .map(|b| b.service)
            .collect();
        assert_eq!(hard.len(), 6, "hard gates: {hard:?}");
        for svc in [
            "ados-supervisor",
            "ados-mavlink-router",
            "ados-video",
            "ados-cloud",
            "ados-vision",
            "ados-control",
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
    fn control_is_hard_on_every_profile() {
        let control = PREBUILT
            .iter()
            .find(|b| b.service == "ados-control")
            .expect("ados-control must be in the catalog");
        // The control surface is the LAN front on :8080 (the residual Python
        // API sits behind it on an internal socket), so a node without it cannot
        // be paired or reached: a fetch miss must fail the install.
        assert_eq!(control.gate, Gate::Hard);
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
    fn onnx_vision_variant_targets_the_same_service_and_destination() {
        // The onnx build is the SAME service installed at the SAME path as the
        // default vision binary — only the fetched asset differs.
        let default = default_vision_binary();
        assert_eq!(PREBUILT_VISION_ONNX.service, default.service);
        assert_eq!(PREBUILT_VISION_ONNX.dest, default.dest);
        assert_eq!(PREBUILT_VISION_ONNX.dest, "/opt/ados/bin/ados-vision");
        // A distinct asset so the default (musl) build is untouched.
        assert_ne!(PREBUILT_VISION_ONNX.asset, default.asset);
        assert_eq!(PREBUILT_VISION_ONNX.asset, "ados-vision-onnx-aarch64");
        // Not part of the catalog (it is selected in place of the default entry).
        assert!(!PREBUILT
            .iter()
            .any(|b| b.asset == PREBUILT_VISION_ONNX.asset));
    }

    #[test]
    fn onnx_runtime_library_ships_beside_the_onnx_binary() {
        // The ONNX Runtime shared library installs off `bin/`, and is
        // best-effort so a miss falls back to the default vision build rather
        // than aborting the install.
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
    fn a_board_override_stem_selects_the_same_vision_build_as_the_model_string() {
        // `/etc/ados/board_override` carries the board-profile YAML filename
        // stem, and this gate reads that string when it is set. A stem that
        // matched nothing here downgraded a deliberately-pinned onnx board to
        // the non-ONNX build with nothing reporting it.
        for stem in ["rpi5", "cm5", "cubie-a7s"] {
            assert!(
                board_prefers_onnx_vision(stem),
                "override stem {stem} must select the onnx vision build"
            );
            assert_eq!(vision_binary(stem).asset, "ados-vision-onnx-aarch64");
        }
        // An NPU-sidecar board's stem still resolves to the default build.
        assert!(!board_prefers_onnx_vision("rock-5c-lite"));
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
