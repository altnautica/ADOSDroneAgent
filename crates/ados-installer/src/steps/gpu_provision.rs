//! GPU provision: enable the HDMI kiosk's GPU renderer for the board's GPU, in
//! one of two ways, then write a render marker the kiosk reads:
//!   - **Rockchip Mali** (the vendor kbase owns the GPU, `/dev/mali0`): install a
//!     SCOPED libmali GPU userspace blob so cage + Chromium can accelerate their
//!     UI compositing WITHOUT replacing the system Mesa libEGL. A running desktop
//!     and the software fallback both stay intact because the blob is exposed
//!     only to the kiosk's own processes.
//!   - **Mesa-native** (Pi VideoCore / Intel / AMD / NVIDIA, where the stock Mesa
//!     GL already drives the GPU): just write the marker with NO scoped lib dir —
//!     the kiosk uses the system GL. No blob, no download.
//! Any other board (or a failed provision) leaves the kiosk on the software
//! renderer, which always works.
//!
//! Non-destructive by design:
//!   1. the `.deb` is fetched (pinned URL + verified sha256) and extracted with
//!      `dpkg-deb -x` — no dpkg database entry, no maintainer scripts, no
//!      `/etc/ld.so.conf.d` touch, no Mesa file replaced,
//!   2. its blob + the EGL/GLES/GBM wrapper shims are flattened into a private
//!      directory (`/opt/ados/gpu/mali`),
//!   3. the kiosk loads them via a SCOPED `LD_LIBRARY_PATH` (set from the render
//!      marker this step writes), so `libEGL`/`libGLESv2`/`libgbm` shadow Mesa
//!      only for cage + Chromium — the system default stays Mesa for everything
//!      else.
//!
//! Optional + fail-soft: a board with no Mali kbase GPU (`/dev/mali0` absent),
//! or a GPU family with no verified blob catalogued, is a clean Skipped; any
//! download / verify / extract failure degrades (the render marker is NOT
//! written, so the kiosk keeps the software renderer that always works).
//! Checkpoint `gpu-provision`.
//!
//! HW *video* decode is deliberately NOT provisioned: Chromium on this distro
//! cannot drive the Rockchip VPU (no patched build exists for the shipped
//! version), so one small video stream is software-decoded (comfortably viable
//! on the CPU) while libmali still accelerates the UI. Claiming a HW-decode
//! surface that does not work would be a false status; software decode is the
//! honest path here.
//!
//! Generic: the `gpu_family -> libmali variant` mapping is a catalog keyed on
//! the SoC's GPU family probed from the device tree, so a new Rockchip Mali SBC
//! is one verified catalog row, not a code change (the prebuilt-driver-matrix
//! discipline). The detection + URL/marker builders are pure so unit tests
//! exercise them without a board, the network, or a GPU.

use std::path::Path;

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;

/// Where the scoped libmali userspace lands (never the system libdir). Flattened
/// so a single-directory `LD_LIBRARY_PATH` resolves both the wrapper shims and
/// the `libmali.so.1` / `libmali-hook.so.1` they need.
const MALI_SCOPE_DIR: &str = "/opt/ados/gpu/mali";
/// The kiosk render marker this step writes on success. The kiosk reads
/// `renderer` + `lib_dir` from it to decide GPU vs software and where the scoped
/// libmali is.
const RENDER_MARKER_PATH: &str = "/etc/ados/kiosk-render.conf";
/// The vendor kbase GPU node; its presence means the Mali driver owns the GPU
/// (the safe runtime GPU probe — a node stat, never an EGL init that could hang
/// a box on a mismatched driver stack).
const MALI_KBASE_NODE: &str = "/dev/mali0";
/// The kernel CSF firmware path. It is paired with the KERNEL kbase, so we only
/// ever fill it when missing and NEVER overwrite a kernel-matched one.
const CSFFW_DEST: &str = "/lib/firmware/mali_csffw.bin";
/// panfrost/panthor blacklist so the vendor kbase keeps the GPU (the Mesa
/// panfrost/panthor drivers cannot drive a Valhall-CSF Mali and would fight the
/// vendor kbase for the device).
const PANFROST_BLACKLIST_PATH: &str = "/etc/modprobe.d/ados-gpu-panfrost.conf";

/// A libmali catalog row: a GPU family -> the pinned userspace blob to provision.
struct LibmaliVariant {
    /// The GPU family key (probed from the device tree).
    family: &'static str,
    /// The `.deb` download URL (pinned release tag).
    url: &'static str,
    /// The `.deb` sha256 (verified; a mismatch aborts the provision fail-soft).
    sha256: &'static str,
}

/// The verified libmali catalog. One row per Rockchip Mali family whose blob's
/// sha256 has been verified (mirrors the prebuilt-driver matrix discipline). An
/// un-catalogued family is a clean Skipped (software renderer) — never a guessed
/// blob.
const LIBMALI_CATALOG: &[LibmaliVariant] = &[
    // RK3588 / RK3588S2 / RK3582 — Mali-G610 (Valhall, CSF). The `-wayland-gbm`
    // variant carries the `libwayland-egl.so.1` a Wayland Chromium client needs;
    // g24p0 is the newest userspace packaged for g610 and is DDK-compatible with
    // the g25p0 vendor kernel.
    LibmaliVariant {
        family: "valhall-g610",
        url: "https://github.com/tsukumijima/libmali-rockchip/releases/download/v1.9-1-20260312-bd33ee2/libmali-valhall-g610-g24p0-wayland-gbm_1.9-1_arm64.deb",
        sha256: "533fc920220f36e0614e0e4808eabea55976c5af84f9b4996971862d183d2b62",
    },
];

/// Map a device-tree compatible/model string to a libmali GPU family key. Pure +
/// case-insensitive; None when the board is not a known Rockchip Mali SoC (the
/// step then Skips). Generic across the Rockchip Mali line — adding a family here
/// plus a verified catalog row is all a new SBC needs.
pub fn gpu_family_for(soc_identity: &str) -> Option<&'static str> {
    let m = soc_identity.to_lowercase();
    // Valhall G610: RK3588 / RK3588S / RK3588S2 / RK3582.
    if m.contains("rk3588") || m.contains("rk3582") {
        return Some("valhall-g610");
    }
    // Bifrost G52: RK3566 / RK3568 / RK3562 / RK3576 (one G52 blob covers all).
    if m.contains("rk3566") || m.contains("rk3568") || m.contains("rk3562") || m.contains("rk3576")
    {
        return Some("bifrost-g52");
    }
    // Valhall G310: RK3572.
    if m.contains("rk3572") {
        return Some("valhall-g310");
    }
    // Midgard T860: RK3399 (arm64).
    if m.contains("rk3399") {
        return Some("midgard-t86x");
    }
    None
}

fn catalog_lookup(family: &str) -> Option<&'static LibmaliVariant> {
    LIBMALI_CATALOG.iter().find(|v| v.family == family)
}

/// The render-marker file body for a provisioned GPU (pure). Kept in sync with
/// the kiosk's marker parser: `renderer` + optional `lib_dir` keys. A scoped
/// `lib_dir` is present for the Rockchip libmali path; the Mesa-native path
/// (Pi / Intel / AMD, where the stock Mesa GL already drives the GPU) writes no
/// `lib_dir` so the kiosk uses the system GL.
fn render_marker_body(lib_dir: Option<&str>) -> String {
    let head = "# Written by the ADOS installer's GPU provisioning step.\n\
                # The HDMI kiosk reads this to select the GPU (gles2/EGL) renderer.\n\
                # Delete it to force the software renderer.\n\
                renderer: gpu\n";
    match lib_dir {
        Some(dir) => format!("{head}lib_dir: {dir}\n"),
        None => head.to_string(),
    }
}

/// DRM render/display drivers whose stock Mesa GL reliably drives the GPU, so
/// the kiosk can use the GPU (cage gles2 + Chromium EGL) with NO vendor blob.
/// Deliberately conservative: it excludes `panfrost`/`lima` because on the
/// Rockchip Valhall boards we target those are the BROKEN path (a Valhall Mali
/// is handled by the libmali route via `/dev/mali0`, not Mesa). `v3d`/`vc4` =
/// Raspberry Pi VideoCore, `i915` = Intel, `amdgpu`/`radeon` = AMD, `nouveau` =
/// NVIDIA.
const MESA_GOOD_DRIVERS: &[&str] = &["v3d", "vc4", "i915", "amdgpu", "radeon", "nouveau"];

/// Return the name of a known-good Mesa GL DRM driver bound to a GPU on this
/// box, or None. Reads the driver symlink of the render/display nodes — a safe
/// stat, never a live GL init. Pure enough to unit-test via the sysfs root.
fn mesa_native_gpu_driver() -> Option<String> {
    for node in [
        "/sys/class/drm/renderD128/device/driver",
        "/sys/class/drm/renderD129/device/driver",
        "/sys/class/drm/card0/device/driver",
        "/sys/class/drm/card1/device/driver",
    ] {
        if let Ok(target) = std::fs::read_link(node) {
            if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                if MESA_GOOD_DRIVERS.contains(&name) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// The panfrost/panthor blacklist file body (pure).
fn panfrost_blacklist_body() -> String {
    "# The vendor Mali kbase driver owns the GPU (/dev/mali0). Mesa's panfrost /\n\
     # panthor cannot drive a Valhall-CSF Mali and must not claim the device.\n\
     blacklist panfrost\n\
     blacklist panthor\n"
        .to_string()
}

/// Read the SoC identity the GPU-family probe keys on: the device-tree
/// `compatible` list (carries the SoC id, e.g. `rockchip,rk3588`) plus the model
/// string. Empty on a dev host (which `gpu_family_for` then treats as unknown).
fn read_soc_identity() -> String {
    let mut s = String::new();
    if let Ok(compat) = std::fs::read_to_string("/proc/device-tree/compatible") {
        s.push_str(&compat.replace('\0', " "));
        s.push(' ');
    }
    if let Ok(model) = std::fs::read_to_string("/proc/device-tree/model") {
        s.push_str(&model.replace('\0', ""));
    }
    s
}

/// True when the vendor Mali kbase driver owns the GPU (its device node exists).
fn mali_kbase_present() -> bool {
    Path::new(MALI_KBASE_NODE).exists()
}

/// sha256 of a file via `sha256sum`, lowercase hex, or None when the tool is
/// absent / the file is unreadable.
fn sha256_of(path: &Path) -> Option<String> {
    let p = path.to_string_lossy();
    let res = exec::run("sha256sum", &[p.as_ref()]);
    if !res.success() {
        return None;
    }
    res.stdout
        .split_whitespace()
        .next()
        .map(|h| h.to_lowercase())
}

/// Fetch the pinned `.deb` to a temp file and verify its sha256. Returns the
/// staged path on success. Fail-soft: any error is a reason string.
fn download_and_verify(variant: &LibmaliVariant) -> Result<std::path::PathBuf, String> {
    let dest = std::env::temp_dir().join("ados-libmali.deb");
    net::fetch(variant.url, &dest).map_err(|e| format!("fetching libmali blob failed: {e}"))?;
    match sha256_of(&dest) {
        Some(got) if got == variant.sha256 => Ok(dest),
        Some(got) => {
            let _ = std::fs::remove_file(&dest);
            Err(format!(
                "libmali blob sha256 mismatch (expected {}, got {got})",
                variant.sha256
            ))
        }
        None => {
            // Without a checksum we cannot trust the blob — refuse it.
            let _ = std::fs::remove_file(&dest);
            Err("could not compute the libmali blob sha256 (sha256sum missing?)".to_string())
        }
    }
}

/// Extract the `.deb` and flatten libmali + the wrapper shims into the scoped
/// dir. Fail-soft: any error is a reason string. Fills the CSF firmware only when
/// the kernel firmware slot is empty (never overwrites a kernel-matched one).
fn extract_scoped(deb: &Path) -> Result<(), String> {
    let extract = std::env::temp_dir().join("ados-libmali-x");
    let _ = std::fs::remove_dir_all(&extract);
    let extract_s = extract.to_string_lossy().into_owned();

    let deb_s = deb.to_string_lossy();
    let res = exec::run("dpkg-deb", &["-x", deb_s.as_ref(), extract_s.as_str()]);
    if !res.success() {
        return Err(format!("dpkg-deb -x failed: {}", res.stderr.trim()));
    }

    // Fresh scope dir, then flatten the blob (top-level) + the wrapper shims
    // (the .deb's `mali/` subdir) into it so a single-dir LD_LIBRARY_PATH works.
    let _ = std::fs::remove_dir_all(MALI_SCOPE_DIR);
    std::fs::create_dir_all(MALI_SCOPE_DIR)
        .map_err(|e| format!("could not create {MALI_SCOPE_DIR}: {e}"))?;

    let libdir = format!("{extract_s}/usr/lib/aarch64-linux-gnu");
    // `cp -a` preserves the versioned symlinks (libmali.so.1 -> ...1.9.0) and the
    // shim ELF files; globs need a shell. The paths are installer-controlled
    // constants / a temp dir, so there is no untrusted input in the command.
    let cp = format!(
        "cp -a {q}/libmali*.so* {dst}/ && cp -a {q}/mali/. {dst}/",
        q = shell_quote(&libdir),
        dst = shell_quote(MALI_SCOPE_DIR),
    );
    let res = exec::run("sh", &["-c", cp.as_str()]);
    if !res.success() {
        let _ = std::fs::remove_dir_all(&extract);
        return Err(format!(
            "copying libmali into the scope dir failed: {}",
            res.stderr.trim()
        ));
    }

    // The blob AND the EGL wrapper cage actually loads must both be present,
    // else refuse (do not write the marker) so a partial flatten falls back to
    // the software renderer cleanly rather than half-arming the GPU path.
    let scope = Path::new(MALI_SCOPE_DIR);
    if !scope.join("libmali.so.1").exists() || !scope.join("libEGL.so.1").exists() {
        let _ = std::fs::remove_dir_all(&extract);
        return Err("libmali.so.1 / libEGL.so.1 missing after extract".to_string());
    }

    // CSF firmware: fill ONLY when the kernel slot is empty. The firmware pairs
    // with the kernel kbase, so an existing one is kept untouched.
    if !Path::new(CSFFW_DEST).exists() {
        let src = format!("{extract_s}/lib/firmware/mali_csffw.bin");
        if Path::new(&src).exists() {
            if let Some(parent) = Path::new(CSFFW_DEST).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(&src, CSFFW_DEST);
        }
    }

    let _ = std::fs::remove_dir_all(&extract);
    Ok(())
}

/// Minimal single-quote shell escaping for a trusted path (defence in depth even
/// though the inputs are installer constants / a temp dir).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn write_file(path: &str, body: &str) -> std::io::Result<()> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, p)
}

pub struct GpuProvision;

impl Step for GpuProvision {
    fn id(&self) -> &str {
        "gpu_provision"
    }
    fn requires(&self) -> &[&str] {
        // deps for dpkg-deb/sha256sum/cp; config_identity so /etc/ados exists
        // and the marker lands before systemd starts the kiosk.
        &["deps", "config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("gpu-provision")
    }
    fn kind(&self) -> StepKind {
        // A GPU accel we could not set up degrades (not aborts): the kiosk works
        // on the software renderer regardless.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // The HDMI kiosk (the sole consumer of the render marker) is a
        // ground-station surface, so GPU provisioning is scoped to it.
        if ctx.profile != "ground_station" {
            return StepOutcome::Skipped;
        }
        // Rockchip Mali (the vendor kbase owns the GPU) -> a scoped libmali blob.
        if mali_kbase_present() {
            return provision_libmali();
        }
        // Otherwise, if the stock Mesa GL already drives the GPU (Pi VideoCore,
        // Intel, AMD, NVIDIA), enable the GPU renderer with NO vendor blob: the
        // marker carries no lib_dir, so the kiosk uses the system GL directly.
        if let Some(driver) = mesa_native_gpu_driver() {
            if let Err(e) = write_file(RENDER_MARKER_PATH, &render_marker_body(None)) {
                return StepOutcome::Failed(format!("writing the render marker failed: {e}"));
            }
            tracing::info!(
                driver,
                "enabled the Mesa GPU renderer for the kiosk (no blob)"
            );
            return StepOutcome::Ok;
        }
        tracing::info!("no accelerated GPU userspace found; kiosk uses the software renderer");
        StepOutcome::Skipped
    }
}

/// Provision the scoped libmali blob for a Rockchip Mali board and write the GPU
/// render marker. Any failure degrades WITHOUT writing the marker, so the kiosk
/// falls back to the software renderer that always works.
fn provision_libmali() -> StepOutcome {
    let family = match gpu_family_for(&read_soc_identity()) {
        Some(f) => f,
        None => {
            tracing::info!("GPU family not recognised; kiosk uses software renderer");
            return StepOutcome::Skipped;
        }
    };
    let variant = match catalog_lookup(family) {
        Some(v) => v,
        None => {
            tracing::info!(
                family,
                "no verified libmali blob catalogued for this GPU family; software renderer"
            );
            return StepOutcome::Skipped;
        }
    };

    let deb = match download_and_verify(variant) {
        Ok(p) => p,
        Err(e) => return StepOutcome::Failed(e),
    };
    let extract_res = extract_scoped(&deb);
    let _ = std::fs::remove_file(&deb);
    if let Err(e) = extract_res {
        return StepOutcome::Failed(e);
    }

    // Keep the vendor kbase in charge of the GPU (best-effort; a write failure
    // here does not undo the provisioned userspace).
    if let Err(e) = write_file(PANFROST_BLACKLIST_PATH, &panfrost_blacklist_body()) {
        tracing::warn!(error = %e, "could not write the panfrost blacklist");
    }

    // The marker is the last thing written: its presence means the scoped
    // libmali is ready for the kiosk to use.
    if let Err(e) = write_file(
        RENDER_MARKER_PATH,
        &render_marker_body(Some(MALI_SCOPE_DIR)),
    ) {
        return StepOutcome::Failed(format!("writing the render marker failed: {e}"));
    }
    tracing::info!(
        family,
        dir = MALI_SCOPE_DIR,
        "provisioned scoped libmali GPU userspace for the kiosk"
    );
    StepOutcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_family_maps_rockchip_mali_socs() {
        assert_eq!(
            gpu_family_for("rockchip,rk3588 Radxa ROCK 5C"),
            Some("valhall-g610")
        );
        assert_eq!(gpu_family_for("rockchip,rk3588s2"), Some("valhall-g610"));
        assert_eq!(gpu_family_for("rockchip,rk3582"), Some("valhall-g610"));
        assert_eq!(gpu_family_for("rockchip,rk3566"), Some("bifrost-g52"));
        assert_eq!(gpu_family_for("rockchip,rk3576"), Some("bifrost-g52"));
        assert_eq!(gpu_family_for("rockchip,rk3399"), Some("midgard-t86x"));
    }

    #[test]
    fn gpu_family_none_for_non_mali_boards() {
        assert_eq!(gpu_family_for("raspberrypi,4-model-b bcm2711"), None);
        assert_eq!(gpu_family_for(""), None);
        assert_eq!(gpu_family_for("some dev host"), None);
    }

    #[test]
    fn every_recognised_family_leading_the_probe_has_a_catalog_or_is_honest() {
        // valhall-g610 is the one verified blob today; the others resolve a
        // family but Skip cleanly (no guessed blob) until their sha is verified.
        assert!(catalog_lookup("valhall-g610").is_some());
        assert!(catalog_lookup("bifrost-g52").is_none());
    }

    #[test]
    fn catalog_url_is_the_wayland_gbm_variant() {
        // A Wayland kiosk needs the -wayland-gbm variant (it carries
        // libwayland-egl.so.1); the plain -gbm variant would not run Chromium.
        let v = catalog_lookup("valhall-g610").unwrap();
        assert!(v.url.contains("wayland-gbm"));
        assert!(v.url.ends_with("_arm64.deb"));
        assert_eq!(v.sha256.len(), 64);
    }

    #[test]
    fn render_marker_body_declares_gpu_and_lib_dir() {
        let body = render_marker_body(Some("/opt/ados/gpu/mali"));
        assert!(body.contains("renderer: gpu"));
        assert!(body.contains("lib_dir: /opt/ados/gpu/mali"));
    }

    #[test]
    fn render_marker_body_no_lib_dir_for_mesa_native() {
        // The Mesa-native path (Pi / Intel / AMD) writes gpu with NO lib_dir so
        // the kiosk uses the system GL (the kiosk treats a missing lib_dir as a
        // system-GL GPU renderer).
        let body = render_marker_body(None);
        assert!(body.contains("renderer: gpu"));
        assert!(!body.contains("lib_dir"));
    }

    #[test]
    fn mesa_good_drivers_include_known_and_exclude_panfrost() {
        // Conservative allowlist: the reliably-Mesa-driven GPUs are enabled, but
        // panfrost/lima are excluded — on the Rockchip Valhall boards we target
        // those are the broken path (a Valhall Mali is handled via libmali).
        for good in ["v3d", "vc4", "i915", "amdgpu", "radeon", "nouveau"] {
            assert!(MESA_GOOD_DRIVERS.contains(&good), "missing {good}");
        }
        assert!(!MESA_GOOD_DRIVERS.contains(&"panfrost"));
        assert!(!MESA_GOOD_DRIVERS.contains(&"lima"));
    }

    #[test]
    fn panfrost_blacklist_covers_both_drivers() {
        let body = panfrost_blacklist_body();
        assert!(body.contains("blacklist panfrost"));
        assert!(body.contains("blacklist panthor"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/opt/ados"), "'/opt/ados'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
