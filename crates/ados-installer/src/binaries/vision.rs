//! The `ados-vision` binary selection: the default build or the ONNX-enabled
//! variant for boards that declare CPU-ONNX local inference, and for NPU boards
//! whose detector fell back to the ONNX CPU backend.

use std::path::Path;

use crate::steps::npu_provision::is_npu_board;

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

/// The `vision.backend` the agent config names, read without the agent's
/// config model. `None` when the file is missing, unreadable, not YAML, or has
/// no string `vision.backend`: the caller then keeps the board-only rule.
pub fn configured_vision_backend(config_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    let backend = doc.get("vision")?.get("backend")?.as_str()?.trim();
    (!backend.is_empty()).then(|| backend.to_ascii_lowercase())
}

/// Whether the board gets the onnx-enabled vision build: a board that declares
/// CPU-ONNX local inference always does, and an NPU board does when its config
/// runs the ONNX CPU backend (the NPU provisioning found no `.rknn` detector for
/// the board's platform and fell back to ONNX, so the default build, which
/// cannot run ONNX, would leave it with no detector). Pure.
pub fn wants_onnx_vision(model: &str, configured_backend: Option<&str>) -> bool {
    board_prefers_onnx_vision(model) || (is_npu_board(model) && configured_backend == Some("onnx"))
}

/// The default `ados-vision` catalog entry (the static musl build, no onnx).
pub fn default_vision_binary() -> &'static PrebuiltBinary {
    PREBUILT
        .iter()
        .find(|b| b.service == "ados-vision")
        .expect("ados-vision is in the catalog")
}

/// The `ados-vision` prebuilt to fetch for a board and its configured vision
/// backend (see [`wants_onnx_vision`]). The fetch step falls back to the default
/// when the onnx variant cannot be fetched.
pub fn vision_binary(model: &str, configured_backend: Option<&str>) -> &'static PrebuiltBinary {
    if wants_onnx_vision(model, configured_backend) {
        &PREBUILT_VISION_ONNX
    } else {
        default_vision_binary()
    }
}
