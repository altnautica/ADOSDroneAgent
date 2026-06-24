//! NPU provision: on an RKNN-class board, install the NPU inference runtime,
//! provision the detector model, turn on the vision config, and enable the
//! NPU sidecar unit — so a fresh install brings the on-board vision pipeline up
//! with zero manual steps. Optional — a board without an NPU is a clean
//! `Skipped`, and any sub-step failure on an NPU board degrades (not aborts) the
//! install. Checkpoint `npu-provision`. Runs after `venv_agent` (the sidecar
//! wheel installs into the agent venv).
//!
//! What this step OWNS on an NPU board:
//!   1. the rknn-toolkit-lite2 wheel installed into the agent venv (the Python
//!      side of the sidecar; the wheel carries only the Python API),
//!   2. the librknnrt runtime library placed at `/usr/lib/librknnrt.so` (a
//!      separate artifact the wheel does NOT bundle; the sidecar unit gates on
//!      its presence),
//!   3. the configured detector model fetched into the model cache (best-effort
//!      — when no detector is configured yet, the runtime + config still
//!      provision and the model is deferred, surfaced honestly in the log),
//!   4. the vision config turned on (`vision.enabled = true`,
//!      `vision.backend = rknn`) via a real config load → merge → save so no
//!      other config field is dropped,
//!   5. the `ados-vision-rknn.service` unit ENABLED (the START is the
//!      supervisor's job; the unit is PartOf the supervisor and self-gates on
//!      the runtime library).
//!
//! The detection (board substring match) and the URL builders are pure so a
//! unit test exercises them without a board, the network, or an interpreter.

use std::path::Path;

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;
use crate::steps::venv_agent::{venv_pip, venv_python};

/// The RKNN runtime + sidecar wheel are pinned to this Rockchip toolkit release.
/// The on-disk `.rknn` detector models are exported with this toolkit version,
/// so the runtime library and the wheel MUST match it or the runtime rejects
/// the model (an init mismatch). Bump all three together.
const RKNN_TOOLKIT_VERSION: &str = "2.3.2";

/// Raw-content base for the pinned toolkit tag. The runtime `.so` and the
/// per-Python sidecar wheels both hang off this tree (they are not published to
/// PyPI). `raw.githubusercontent.com` serves the file bytes directly (the repo
/// does not store them under Git LFS at this tag).
const RKNN_RAW_BASE: &str = "https://raw.githubusercontent.com/airockchip/rknn-toolkit2";

/// The destination the sidecar unit gates on (`ConditionPathExists`). The RKNN
/// runtime resolves `librknnrt` from the standard library path at load time.
const LIBRKNNRT_DEST: &str = "/usr/lib/librknnrt.so";

/// Board-model substrings that identify an NPU-class board the agent provisions
/// the RKNN/TensorRT inference path for. Matched case-insensitively against the
/// device-tree model string. The Rockchip RK3588/RK3582/RK3576 family and the
/// boards built on them carry the 6-TOPS NPU the sidecar targets; the Jetson
/// family (Orin/Tegra) is included so the same step gate is the single place the
/// NPU decision lives (its own runtime path layers on the TensorRT sidecar).
const NPU_BOARD_SUBSTRINGS: &[&str] = &[
    "rk3588",
    "rk3582",
    "rk3576",
    "rock-5c",
    "rock5c",
    "orange-pi-5",
    "orangepi5",
    "orange pi 5",
    "jetson",
    "orin",
    "tegra",
];

/// True when the board-model string names an NPU-class board. Case-insensitive
/// substring match so a SoC string ("RK3588"), a board slug ("rock-5c-lite"),
/// or a display name ("Radxa ROCK 5C") all resolve. Pure.
pub fn is_npu_board(model: &str) -> bool {
    let m = model.to_lowercase();
    NPU_BOARD_SUBSTRINGS.iter().any(|k| m.contains(k))
}

/// Read the board-model string the NPU decision keys on: the persisted override
/// sentinel first (an operator/installer force), then the kernel device-tree
/// model. Mirrors the binary's `board_id()` resolution order. Empty when neither
/// is present (a dev host) — which `is_npu_board` then treats as not-NPU.
fn read_board_model() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/ados/board_override") {
        let v = s.trim().trim_matches('\0').trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/device-tree/model") {
        let v = s.replace('\0', "");
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    String::new()
}

/// Map a venv Python `(major, minor)` to the CPython wheel tag (`cp311`) of a
/// published rknn-toolkit-lite2 aarch64 wheel for the pinned toolkit version.
/// Returns `None` when no wheel is published for that interpreter (e.g. a 3.13+
/// venv at toolkit 2.3.2, which ships cp310–cp312), so the caller fails the
/// provision LOUDLY with an actionable reason rather than installing nothing and
/// reporting success. Pure.
pub fn wheel_cp_tag(major: u32, minor: u32) -> Option<String> {
    if major != 3 {
        return None;
    }
    // The 2.3.2 aarch64 wheels published for the toolkit tag.
    if (7..=12).contains(&minor) {
        Some(format!("cp3{minor}"))
    } else {
        None
    }
}

/// The rknn-toolkit-lite2 aarch64 wheel filename for a CPython tag (pure). The
/// cp37 wheel uses the `cp37m` ABI tag; every later tag repeats the cp tag.
pub fn wheel_filename(cp_tag: &str) -> String {
    let abi = if cp_tag == "cp37" { "cp37m" } else { cp_tag };
    format!(
        "rknn_toolkit_lite2-{RKNN_TOOLKIT_VERSION}-{cp_tag}-{abi}-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
    )
}

/// The rknn-toolkit-lite2 aarch64 wheel download URL for a CPython tag (pure).
pub fn wheel_url(cp_tag: &str) -> String {
    format!(
        "{RKNN_RAW_BASE}/v{RKNN_TOOLKIT_VERSION}/rknn-toolkit-lite2/packages/{}",
        wheel_filename(cp_tag)
    )
}

/// The librknnrt runtime library download URL for the pinned toolkit (pure).
pub fn librknnrt_url() -> String {
    format!(
        "{RKNN_RAW_BASE}/v{RKNN_TOOLKIT_VERSION}/rknpu2/runtime/Linux/librknn_api/aarch64/librknnrt.so"
    )
}

/// Resolve the venv interpreter's `(major, minor)` by asking it. Returns `None`
/// when the venv python cannot be run (it should exist — `venv_agent` ran first).
fn venv_python_version() -> Option<(u32, u32)> {
    let res = exec::run(
        &venv_python(),
        &[
            "-c",
            "import sys; print(f'{sys.version_info.major} {sys.version_info.minor}')",
        ],
    );
    if !res.success() {
        return None;
    }
    let mut it = res.stdout.split_whitespace();
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// Install the rknn-toolkit-lite2 wheel into the agent venv. Fetches the
/// arch+Python-matched wheel from the pinned toolkit tag and `pip install`s the
/// local file. Returns an actionable error string (not a panic) on any failure.
fn install_rknn_wheel() -> Result<(), String> {
    let (major, minor) = venv_python_version()
        .ok_or_else(|| "could not query the agent venv Python version".to_string())?;
    let cp_tag = wheel_cp_tag(major, minor).ok_or_else(|| {
        format!(
            "no rknn-toolkit-lite2 {RKNN_TOOLKIT_VERSION} wheel is published for Python {major}.{minor} \
             (the toolkit ships cp37–cp312 aarch64 wheels); provision the agent venv with Python \
             3.11 or 3.12 on an NPU board so the NPU runtime can install"
        )
    })?;

    let url = wheel_url(&cp_tag);
    let dest = std::env::temp_dir().join(wheel_filename(&cp_tag));
    net::fetch(&url, &dest).map_err(|e| format!("fetching the rknn wheel failed: {e}"))?;

    let dest_s = dest.to_string_lossy().into_owned();
    let res = exec::run(&venv_pip(), &["install", "--quiet", &dest_s]);
    let _ = std::fs::remove_file(&dest);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        Err(format!("venv pip {} could not be spawned", venv_pip()))
    } else {
        Err(format!(
            "pip install of the rknn-toolkit-lite2 wheel failed: {}",
            res.stderr.trim()
        ))
    }
}

/// Provision the librknnrt runtime library at `/usr/lib/librknnrt.so` (the path
/// the sidecar unit gates on). Idempotent: re-fetches to a temp file and
/// atomically replaces, so an `--upgrade` to a new toolkit refreshes the runtime
/// in lockstep with the wheel. Returns an actionable error string on failure.
fn provision_librknnrt() -> Result<(), String> {
    let url = librknnrt_url();
    let tmp = std::env::temp_dir().join("librknnrt.so.download");
    net::fetch(&url, &tmp).map_err(|e| format!("fetching librknnrt.so failed: {e}"))?;

    // A truncated/empty download must never land as the runtime library.
    match std::fs::metadata(&tmp) {
        Ok(meta) if meta.len() > 0 => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&tmp);
            return Err("downloaded librknnrt.so is empty".to_string());
        }
        Err(e) => return Err(format!("staged librknnrt.so is unreadable: {e}")),
    }

    if let Some(parent) = Path::new(LIBRKNNRT_DEST).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Move into place atomically when on the same filesystem; fall back to a
    // copy (the temp dir may be a different mount than /usr/lib).
    let placed = std::fs::rename(&tmp, LIBRKNNRT_DEST).is_ok()
        || std::fs::copy(&tmp, LIBRKNNRT_DEST).is_ok();
    let _ = std::fs::remove_file(&tmp);
    if !placed {
        return Err(format!("placing librknnrt.so at {LIBRKNNRT_DEST} failed"));
    }
    set_mode(Path::new(LIBRKNNRT_DEST), 0o644);
    // Refresh the dynamic linker cache so the new library is resolvable without
    // a reboot (best-effort; absent on a stripped container).
    let _ = exec::run("ldconfig", &[]);
    Ok(())
}

/// The inline Python that fetches the configured detector model into the cache.
/// It loads the live config, picks the detector model id (the configured
/// `vision.detector`, else the first detection-task model the registry lists),
/// and runs ModelManager.download_model for it. Prints a single status token so
/// the caller can tell "fetched" from "deferred (none configured)" from "error".
const FETCH_DETECTOR_PY: &str = r#"
import asyncio, sys
from ados.core.config import load_config
from ados.services.vision.model_manager import ModelManager

async def main():
    cfg = load_config()
    vision = cfg.vision
    # Prefer an explicitly-configured detector model id (forward-compatible: the
    # field may not exist on an older config model). Fall back to the first
    # detection-task model the registry advertises.
    model_id = getattr(vision, "detector", None) or ""
    mgr = ModelManager(vision)
    if not model_id:
        registry = await mgr.fetch_registry()
        for m in registry:
            if (m.task or "").lower() == "detection":
                model_id = m.id
                break
    if not model_id:
        print("DEFERRED")
        return
    path = await mgr.download_model(model_id)
    print("FETCHED " + str(path))

try:
    asyncio.run(main())
except Exception as exc:  # surface a clean reason to the installer log
    print("ERROR " + repr(exc), file=sys.stderr)
    sys.exit(2)
"#;

/// Fetch the configured detector model into the model cache (best-effort). A
/// missing detector config is NOT a failure — the runtime + config still
/// provision, and the model is deferred until one is configured (logged, never a
/// silent half-arm). A real fetch error on a configured model IS surfaced so the
/// install degrades honestly.
fn fetch_detector_model() -> Result<(), String> {
    let res = exec::run(&venv_python(), &["-c", FETCH_DETECTOR_PY]);
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to fetch the detector model".to_string(),
        );
    }
    let out = res.stdout.trim();
    if out.starts_with("FETCHED") {
        tracing::info!(detail = out, "detector model fetched into the cache");
        Ok(())
    } else if out == "DEFERRED" {
        tracing::info!(
            "no detector model configured; the NPU runtime + vision config are provisioned and \
             the model will resolve once a detector is configured"
        );
        Ok(())
    } else {
        Err(format!(
            "fetching the detector model failed: {}",
            res.stderr.trim()
        ))
    }
}

/// The inline Python that turns on the vision config. It loads the on-disk
/// config dict, deep-merges a `vision` override (enabled + rknn backend) into
/// it, and atomically rewrites the file — so every other field is preserved
/// (NOT a model_dump-with-defaults rewrite that would inject every default).
const ENABLE_VISION_PY: &str = r#"
import os, sys, yaml

CONFIG = "/etc/ados/config.yaml"

def deep_merge(base, override):
    out = dict(base)
    for k, v in override.items():
        if k in out and isinstance(out[k], dict) and isinstance(v, dict):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = v
    return out

raw = {}
if os.path.isfile(CONFIG):
    with open(CONFIG) as fh:
        loaded = yaml.safe_load(fh)
    if isinstance(loaded, dict):
        raw = loaded

merged = deep_merge(raw, {"vision": {"enabled": True, "backend": "rknn"}})

body = yaml.safe_dump(merged, sort_keys=False, default_flow_style=False)
os.makedirs(os.path.dirname(CONFIG), exist_ok=True)
tmp = CONFIG + ".tmp"
with open(tmp, "w") as fh:
    fh.write(body)
os.replace(tmp, CONFIG)
print("OK")
"#;

/// Turn on the vision config (`vision.enabled = true`, `vision.backend = rknn`)
/// via a real config load → merge → save through the inline writer. Returns an
/// actionable error string on any failure.
fn enable_vision_config() -> Result<(), String> {
    let res = exec::run(&venv_python(), &["-c", ENABLE_VISION_PY]);
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to write the vision config".to_string(),
        );
    }
    if res.success() && res.stdout.trim() == "OK" {
        Ok(())
    } else {
        Err(format!(
            "writing the vision config failed: {}",
            res.stderr.trim()
        ))
    }
}

/// Enable the NPU sidecar unit (the START is the supervisor's job; the unit is
/// PartOf the supervisor and self-gates on the runtime library). Returns an
/// actionable error string when the enable fails.
fn enable_sidecar_unit() -> Result<(), String> {
    const UNIT: &str = "ados-vision-rknn.service";
    let res = exec::run("systemctl", &["enable", UNIT]);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        Err("systemctl is not available to enable the NPU sidecar unit".to_string())
    } else {
        Err(format!("enabling {UNIT} failed: {}", res.stderr.trim()))
    }
}

/// Set a file's permission bits (Unix only; a no-op elsewhere).
fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// Provision the on-board NPU inference path on RKNN-class boards.
pub struct NpuProvision;

impl Step for NpuProvision {
    fn id(&self) -> &str {
        "npu_provision"
    }
    fn requires(&self) -> &[&str] {
        &["venv_agent"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("npu-provision")
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let model = read_board_model();
        if !is_npu_board(&model) {
            tracing::info!(
                board = %model,
                "no NPU detected; skipping NPU runtime provisioning"
            );
            return StepOutcome::Skipped;
        }
        tracing::info!(board = %model, "NPU board detected; provisioning the inference runtime");

        // 1. The Python side of the sidecar (the rknn-toolkit-lite2 wheel).
        if let Err(e) = install_rknn_wheel() {
            return StepOutcome::Failed(e);
        }
        // 2. The runtime library the sidecar unit gates on.
        if let Err(e) = provision_librknnrt() {
            return StepOutcome::Failed(e);
        }
        // 3. The detector model (best-effort; a missing config is deferred, not
        //    a failure — the runtime + config still provision).
        if let Err(e) = fetch_detector_model() {
            return StepOutcome::Failed(e);
        }
        // 4. Turn the vision config on (load → merge → save, no field dropped).
        if let Err(e) = enable_vision_config() {
            return StepOutcome::Failed(e);
        }
        // 5. Enable the sidecar unit (the supervisor starts it).
        if let Err(e) = enable_sidecar_unit() {
            return StepOutcome::Failed(e);
        }

        tracing::info!("NPU inference runtime provisioned");
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npu_boards_match_across_soc_slug_and_display_name() {
        // A SoC string, a board slug, and a full display name all resolve.
        assert!(is_npu_board("rk3588"));
        assert!(is_npu_board("Radxa ROCK 5C Lite (RK3582)"));
        assert!(is_npu_board("rock-5c-lite"));
        assert!(is_npu_board("Orange Pi 5 Plus"));
        assert!(is_npu_board("NVIDIA Jetson Orin Nano"));
        assert!(is_npu_board("rk3576"));
    }

    #[test]
    fn non_npu_boards_do_not_match() {
        // The small-NPU / no-NPU boards the sidecar does not target.
        assert!(!is_npu_board("Raspberry Pi 4 Model B"));
        assert!(!is_npu_board("Raspberry Pi Compute Module 4"));
        assert!(!is_npu_board("rk3566"));
        assert!(!is_npu_board(""));
        assert!(!is_npu_board("unknown"));
    }

    #[test]
    fn wheel_cp_tag_maps_supported_pythons_and_rejects_others() {
        // The 2.3.2 toolkit ships cp37–cp312 aarch64 wheels.
        assert_eq!(wheel_cp_tag(3, 11).as_deref(), Some("cp311"));
        assert_eq!(wheel_cp_tag(3, 12).as_deref(), Some("cp312"));
        assert_eq!(wheel_cp_tag(3, 10).as_deref(), Some("cp310"));
        assert_eq!(wheel_cp_tag(3, 7).as_deref(), Some("cp37"));
        // 3.13+ has no published wheel at this toolkit version → None (the
        // caller fails loudly with an actionable reason, never a silent skip).
        assert_eq!(wheel_cp_tag(3, 13), None);
        // A non-3 major never matches.
        assert_eq!(wheel_cp_tag(2, 7), None);
    }

    #[test]
    fn wheel_filename_uses_the_abi_tag() {
        // Every cp tag >= 3.8 repeats the cp tag as the ABI tag.
        assert_eq!(
            wheel_filename("cp311"),
            "rknn_toolkit_lite2-2.3.2-cp311-cp311-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
        );
        // cp37 uses the cp37m ABI tag.
        assert_eq!(
            wheel_filename("cp37"),
            "rknn_toolkit_lite2-2.3.2-cp37-cp37m-manylinux_2_17_aarch64.manylinux2014_aarch64.whl"
        );
    }

    #[test]
    fn urls_pin_the_toolkit_version_and_arch() {
        let w = wheel_url("cp311");
        assert!(w.contains("/v2.3.2/rknn-toolkit-lite2/packages/"));
        assert!(w.contains("cp311"));
        assert!(w.ends_with(".whl"));

        let lib = librknnrt_url();
        assert!(lib.contains("/v2.3.2/rknpu2/runtime/Linux/librknn_api/aarch64/librknnrt.so"));
        assert!(lib.starts_with("https://"));
    }
}
