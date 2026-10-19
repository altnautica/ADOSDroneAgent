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
fn catalog_has_twenty_seven_entries() {
    assert_eq!(PREBUILT.len(), 27);
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
