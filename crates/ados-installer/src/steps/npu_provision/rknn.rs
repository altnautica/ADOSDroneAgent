//! The RKNN toolkit wheel and `librknnrt` runtime provisioning.

use std::path::Path;

use ados_term::exec;

use crate::net;
use crate::steps::venv_agent::{venv_pip, venv_python};

use super::set_mode;

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
/// the RKNN inference path for. Matched case-insensitively against the
/// device-tree model string. The Rockchip RK3588/RK3582/RK3576 family and the
/// boards built on them carry the 6-TOPS NPU the sidecar targets. Jetson boards
/// are deliberately absent: there is no in-tree accelerated backend for them,
/// so they are not provisioned as an NPU board.
const NPU_BOARD_SUBSTRINGS: &[&str] = &[
    "rk3588",
    "rk3582",
    "rk3576",
    "rock-5c",
    "rock5c",
    "orange-pi-5",
    "orangepi5",
    "orange pi 5",
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
/// is present (a dev host) — which `is_npu_board` then treats as not-NPU. Shared
/// with the fetch step, which keys the vision-binary variant on the same string.
pub(crate) fn read_board_model() -> String {
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
pub(super) fn install_rknn_wheel() -> Result<(), String> {
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
pub(super) fn provision_librknnrt() -> Result<(), String> {
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
