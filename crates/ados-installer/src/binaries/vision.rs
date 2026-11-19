//! The `ados-vision` binary selection: the default build or the ONNX-enabled
//! variant for boards that declare CPU-ONNX local inference.

use super::{Gate, PrebuiltBinary, DRONE_VISOR, PREBUILT};

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
    // Its own asset name: it lands at the same destination as the default vision
    // binary, so publishing it as `ados-vision-<arch>` would collide with it.
    asset_stem: Some("ados-vision-onnx"),
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
    asset_stem: None,
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
    "dragon q6a",
    "dragon-q6a",
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
