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
    /// `workstation`, `compute`, `visor`).
    pub profiles: &'static [&'static str],
}

// Profile constants — kept as slices so the const table can reference them.
/// The flight-controller-bearing profiles that run the swarm, RC and
/// config-over-radio lanes.
const FC: &[&str] = &["drone", "ground_station"];
/// The profiles that talk MAVLink to a flight controller or a USB sensor hub.
const FC_VISOR: &[&str] = &["drone", "ground_station", "visor"];
/// The camera-owning profiles.
const DRONE_VISOR: &[&str] = &["drone", "visor"];
const DRONE: &[&str] = &["drone"];
const GROUND: &[&str] = &["ground_station"];
/// The profiles with a local access point, status panel and buttons.
const GS_VISOR: &[&str] = &["ground_station", "visor"];
/// Every profile, including the workstation-class nodes. Used for the
/// profile-agnostic core services every node needs (orchestrator, cloud relay,
/// control front, plugin host, logging, TUI) so a `--profile workstation` or
/// `--profile compute` install fetches them too.
const ANY: &[&str] = ados_protocol::contracts::PROFILES;

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
        // binary unconditionally and has no Python fallback. Hard on every
        // MAVLink profile so a fetch miss aborts the install instead of shipping
        // a unit that crash-loops with no FC or sensor-hub link.
        gate: Gate::Hard,
        profiles: FC_VISOR,
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
        profiles: DRONE_VISOR,
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
    // The network daemon, exec'd by `ados-uplink-router.service` on the
    // ground station and the visor: uplinks, the Wi-Fi station and the setup
    // access point (it renders `/etc/ados/hostapd-gs.conf`, which
    // `ados-hostapd.service` starts from), so the AP only comes up where the
    // uplink-router unit runs.
    PrebuiltBinary {
        service: "ados-net",
        asset: "ados-net-aarch64",
        dest: "/opt/ados/bin/ados-net",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-pic",
        asset: "ados-pic-aarch64",
        dest: "/opt/ados/bin/ados-pic",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-input",
        asset: "ados-input-aarch64",
        dest: "/opt/ados/bin/ados-input",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-display",
        asset: "ados-display-aarch64",
        dest: "/opt/ados/bin/ados-display",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-display-probe",
        asset: "ados-display-probe-aarch64",
        dest: "/opt/ados/bin/ados-display-probe",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-oled-i2c",
        asset: "ados-oled-i2c-aarch64",
        dest: "/opt/ados/bin/ados-oled-i2c",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    PrebuiltBinary {
        service: "ados-vision",
        asset: "ados-vision-aarch64",
        dest: "/opt/ados/bin/ados-vision",
        gate: Gate::Hard,
        profiles: DRONE_VISOR,
    },
    // The decentralized swarm state bus. Fetched on both FC-bearing profiles,
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
        profiles: FC,
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
    // header GPIO can drive an indicator on an air, a ground or a handheld node.
    PrebuiltBinary {
        service: "ados-gpio",
        asset: "ados-gpio-aarch64",
        dest: "/opt/ados/bin/ados-gpio",
        gate: Gate::BestEffort,
        profiles: FC_VISOR,
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
        profiles: FC,
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
        profiles: FC,
    },
    // The sensor plane. Best-effort: each unit's ExecStart guard execs
    // /bin/true until its binary lands, so a missing asset degrades capture
    // timing and sensor streams, never the C2 path. The clock brokers the
    // sensor bus on every profile with a MAVLink link; the USB sensor hub is
    // camera-node hardware; directly attached GNSS receivers live on the
    // ground node and the visor.
    PrebuiltBinary {
        service: "ados-clock",
        asset: "ados-clock-aarch64",
        dest: "/opt/ados/bin/ados-clock",
        gate: Gate::BestEffort,
        profiles: FC_VISOR,
    },
    PrebuiltBinary {
        service: "ados-sensorhub",
        asset: "ados-sensorhub-aarch64",
        dest: "/opt/ados/bin/ados-sensorhub",
        gate: Gate::BestEffort,
        profiles: DRONE_VISOR,
    },
    PrebuiltBinary {
        service: "ados-gnss",
        asset: "ados-gnss-aarch64",
        dest: "/opt/ados/bin/ados-gnss",
        gate: Gate::BestEffort,
        profiles: GS_VISOR,
    },
    // The LoRa beacon lane. Best-effort and opt-in: the unit gates on the
    // lora-enabled marker (mirroring lora.enabled) and its ExecStart guard
    // execs /bin/true until the binary lands. Fetched where the lane's unit
    // runs: the drone and the ground station.
    PrebuiltBinary {
        service: "ados-lora",
        asset: "ados-lora-aarch64",
        dest: "/opt/ados/bin/ados-lora",
        gate: Gate::BestEffort,
        profiles: FC,
    },
    // The video relay the pipeline streams through. It is a mirrored
    // third-party binary rather than an `ados-*` service, so it lands in the
    // system bin dir. Best-effort: a missing relay degrades video without
    // aborting the install (the health gate verifies its presence separately).
    // Fetched wherever a video pipeline runs: `ados-video` (drone, visor)
    // cannot publish without it, and the ground-side receive plane serves
    // through it.
    PrebuiltBinary {
        service: "mediamtx",
        asset: "mediamtx-aarch64",
        dest: "/usr/local/bin/mediamtx",
        gate: Gate::BestEffort,
        profiles: FC_VISOR,
    },
];

/// The subset of the catalog needed by `profile`
/// (`drone` | `ground_station` | `workstation` | `compute` | `visor`).
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
    profiles: DRONE_VISOR,
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
    profiles: DRONE_VISOR,
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
mod profile_tests;
#[cfg(test)]
mod tests;
