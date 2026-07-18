//! Fetch binaries: download + verify the prebuilt Rust service binaries for
//! the active profile and install them under `/opt/ados/bin`, then install the
//! global `ados*` symlinks. Required. Checkpoint `global-symlinks`.
//!
//! The load-bearing ordering invariant the whole crate exists to guarantee:
//! a Hard-gate binary (supervisor / video / cloud / vision) that cannot be
//! fetched-or-verified makes this step return [`StepOutcome::Failed`], so the
//! graph aborts BEFORE the systemd step runs. A best-effort binary that fails
//! is logged and skipped — the agent still comes up and reports the missing
//! capability.

use std::path::{Path, PathBuf};

use crate::binaries::{self, Gate, PrebuiltBinary};
use crate::ctx::Ctx;
use crate::env;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;
use crate::ui::{activity, ProgressSink};
use crate::verify::{self, Channel};

/// GitHub release-download base; each prebuilt asset hangs off
/// `<base>/<release_tag>/<asset>` (plus `.sha256` / `.minisig` sidecars).
const RELEASE_BASE: &str = "https://github.com/altnautica/ADOSDroneAgent/releases/download";

/// The trust anchor for prebuilt-binary signatures: the public half of the
/// keypair CI signs each asset's `.minisig` with (the private half is the
/// `ADOS_DRIVER_SIGNING_KEY` CI secret). EMBEDDED, not fetched, so a MITM on the
/// release host cannot swap the key. The default `edge` channel stays
/// dev-tolerant (signature skipped, SHA256-only); on `stable` the `.minisig` is
/// mandatory and verified against this key. Verification is dormant until CI is
/// signing (no `.minisig` published → SHA256-only) and activates automatically
/// once a signed release exists. Key id `8DEB4E827E9D083F` (rotated 2026-07).
const ADOS_BINARY_PUBKEY: &str = "RWQ/CJ1+gk7rjVfGSoy6MOL50e8TmO30KD/J+goaEj+WMI1uzEf92rHN";

/// What to do with one binary's fetch-or-verify outcome, keyed off its catalog
/// gate. Pure: a Hard gate's failure aborts the install; a BestEffort gate's
/// failure degrades it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Proceed (success, or a best-effort miss we tolerate).
    Continue,
    /// A Hard-gated binary failed — the install must abort before systemd.
    FailRequired,
}

/// Map a service name + its fetch/verify success to a [`Decision`], keyed off
/// the catalog gate. Pure + unit-testable: no network, no catalog lookup beyond
/// the gate. A Hard gate failing → `FailRequired`; everything else `Continue`.
pub fn gate_outcome(gate: Gate, ok: bool) -> Decision {
    match (gate, ok) {
        (_, true) => Decision::Continue,
        (Gate::Hard, false) => Decision::FailRequired,
        (Gate::BestEffort, false) => Decision::Continue,
    }
}

/// Resolve the channel enum from the ctx's channel string. Anything that is not
/// exactly `stable` is treated as edge (the default + dev path).
fn channel_of(ctx: &Ctx) -> Channel {
    if ctx.channel == "stable" {
        Channel::Stable
    } else {
        Channel::Edge
    }
}

/// On edge we tolerate unsigned artifacts (CI may not yet sign); on stable a
/// signature is mandatory, so allow_unsigned is false there.
fn allow_unsigned_for(channel: Channel) -> bool {
    matches!(channel, Channel::Edge)
}

/// Fetch + verify one prebuilt binary, then place it atomically at its
/// destination. Returns `Ok(())` on success, `Err` on any fetch/verify/place
/// miss (the caller maps that through the gate). `tmp_dir` holds nothing for the
/// binary itself — the binary is fetched to a `.dl` sibling of the real dest so
/// the final placement is a same-filesystem `rename` (see [`place_binary`]); the
/// dir is retained for callers that want a scratch root and for symmetry.
fn install_one(
    b: &PrebuiltBinary,
    _tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    let asset_url = format!("{RELEASE_BASE}/{}/{}", b.release_tag, b.asset);
    let dest = Path::new(b.dest);

    // Ensure /opt/ados/bin exists so the `.dl` sibling and the final rename land
    // on the same filesystem as the destination (atomic rename requires it).
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {} failed: {e}", parent.display()))?;
    }

    // Fetch the binary + its sidecars to siblings of the real dest. The `.sha256`
    // MUST sit next to the binary we verify because `verify_artifact` looks for
    // `<artifact>.sha256` beside the artifact. The `.minisig` is best-effort so
    // verification upgrades to signature-checked automatically once CI signs.
    let dl_bin = dl_sibling(dest);
    let dl_sha = sidecar_path(&dl_bin, "sha256");
    let dl_sig = sidecar_path(&dl_bin, "minisig");

    let outcome = (|| {
        // Stream byte progress so the live pane shows "<service> 4.2/8.1 MB".
        net::fetch_with_progress(&asset_url, &dl_bin, |done, total| {
            sink.byte_progress("fetch_binaries", done, total, b.service);
        })?;
        net::fetch(&format!("{asset_url}.sha256"), &dl_sha)?;
        let _ = net::fetch(&format!("{asset_url}.minisig"), &dl_sig);

        // Verify the downloaded temp BEFORE it is placed at the live path. Edge
        // stays SHA256-only (allow_unsigned short-circuits before the key is
        // used); stable checks the `.minisig` against the vendored trust anchor.
        verify::verify_artifact(
            &dl_bin,
            Some(ADOS_BINARY_PUBKEY),
            channel,
            allow_unsigned_for(channel),
        )?;

        // Name what landed (with its size) in the running step's log tail — this
        // replaces the old repeated generic "installed prebuilt binary" line.
        let size = std::fs::metadata(&dl_bin).map(|m| m.len()).unwrap_or(0);
        sink.sub_log(
            "fetch_binaries",
            &format!("✓ {} {}", b.service, activity::fmt_bytes(size)),
        );

        // chmod the temp, then atomically swap it over the (possibly running)
        // destination. A live process keeps its old inode through the rename.
        set_executable(&dl_bin)?;
        place_binary(&dl_bin, dest)?;
        Ok(())
    })();

    // Always clear the sidecars; clear the `.dl` binary too if we did not place
    // it (a successful `place_binary` already renamed it away).
    let _ = std::fs::remove_file(&dl_sha);
    let _ = std::fs::remove_file(&dl_sig);
    if outcome.is_err() {
        let _ = std::fs::remove_file(&dl_bin);
    }
    outcome
}

/// Fetch + verify + place one binary, retrying on failure with exponential
/// backoff. A single attempt's curl `--retry` (with `--continue-at -` resume)
/// already recovers a short drop mid-transfer; this outer loop adds spaced
/// retries so a longer management-link outage during one binary does not doom
/// the whole install (the field failure on a flaky USB WiFi where one of ~15
/// binaries dropped and aborted the install). Bounded so a genuinely
/// unreachable asset still fails instead of stalling forever.
fn install_one_with_retry(
    b: &PrebuiltBinary,
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut backoff = std::time::Duration::from_secs(1);
    for attempt in 1..=MAX_ATTEMPTS {
        match install_one(b, tmp_dir, channel, sink) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    service = b.service,
                    attempt,
                    backoff_s = backoff.as_secs(),
                    error = %e,
                    "prebuilt binary fetch/verify attempt failed; retrying after backoff"
                );
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, std::time::Duration::from_secs(30));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop returns Ok or Err on the final attempt")
}

/// Install one service's binary. For the vision engine, a board that declares
/// CPU-ONNX local inference (a strong CPU, no NPU) fetches the onnx-enabled build
/// so it runs the detector on the CPU; if that variant cannot be fetched the
/// install falls back to the default build so it never aborts on a missing
/// variant (Rule 26 — the default build still installs and honestly reports no
/// real inference until the onnx variant is available). Every other service
/// installs its single catalog binary unchanged.
fn install_service(
    b: &PrebuiltBinary,
    board_model: &str,
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    if b.service == "ados-vision" && binaries::board_prefers_onnx_vision(board_model) {
        // The onnx binary links the ONNX Runtime dynamically, so the binary AND
        // its shared library are installed together — either both land or the
        // install falls back to the default (musl, no-onnx) build. Installing the
        // onnx binary without its runtime would leave a vision service that
        // cannot dlopen ORT at start.
        match install_onnx_vision(tmp_dir, channel, sink) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "onnx vision build fetch failed; falling back to the default vision build"
                );
                sink.sub_log(
                    "fetch_binaries",
                    "onnx vision build unavailable; using the default vision build",
                );
            }
        }
    }
    install_one_with_retry(b, tmp_dir, channel, sink)
}

/// Install the onnx-enabled `ados-vision` binary together with the ONNX Runtime
/// shared library it dlopens at start. Both must land — if the runtime library
/// cannot be fetched, the onnx binary would fail to start, so this returns `Err`
/// and the caller falls back to the default vision build.
fn install_onnx_vision(
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    install_one_with_retry(&binaries::PREBUILT_VISION_ONNX, tmp_dir, channel, sink)?;
    install_one_with_retry(
        &binaries::PREBUILT_VISION_ONNX_RUNTIME,
        tmp_dir,
        channel,
        sink,
    )
    .map_err(|e| anyhow::anyhow!("ONNX Runtime library fetch failed: {e}"))
}

/// `<dest>.dl` sibling used as the verify-then-rename staging path. It lives in
/// the same directory as `dest` so the final `rename` is atomic.
fn dl_sibling(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".dl");
    PathBuf::from(s)
}

/// `<path>.<ext>` sidecar next to `path` (matches `verify_artifact`'s lookup).
fn sidecar_path(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Atomically place the verified, already-chmod'd `src` at `dest`. A same-dir
/// `rename` swaps the inode in one step: it is never a half-written file, and a
/// running service that has the old binary mmap'd keeps its old inode (no
/// `ETXTBSY`, no `O_TRUNC` on a live executable). Falls back to a copy + chmod
/// only if the rename fails (e.g. a cross-filesystem dest the caller forced).
fn place_binary(src: &Path, dest: &Path) -> anyhow::Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Non-atomic fallback for a dest on a different filesystem.
            std::fs::copy(src, dest).map_err(|e| {
                anyhow::anyhow!("copy {} -> {} failed: {e}", src.display(), dest.display())
            })?;
            set_executable(dest)?;
            let _ = std::fs::remove_file(src);
            Ok(())
        }
    }
}

/// chmod 0755 (Unix); a no-op stub on non-Unix dev hosts.
#[cfg(unix)]
fn set_executable(dest: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(dest, perms)
        .map_err(|e| anyhow::anyhow!("chmod 0755 {} failed: {e}", dest.display()))
}

#[cfg(not(unix))]
fn set_executable(_dest: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Install the global `/usr/local/bin/ados*` symlinks (the genuine "symlinks"
/// part). `ados` + `ados-agent` point into the venv's console scripts;
/// `ados-supervisor` points at the Rust binary under `/opt/ados/bin` so the
/// operator command is on PATH. This set mirrors the uninstall removal list so
/// the two surfaces never drift. Best-effort: a symlink failure does not abort
/// the install (the binaries are already on disk), but it is logged.
fn install_global_symlinks() {
    let pairs = [
        (format!("{}/bin/ados", env::VENV_DIR), "/usr/local/bin/ados"),
        (
            format!("{}/bin/ados-agent", env::VENV_DIR),
            "/usr/local/bin/ados-agent",
        ),
        (
            format!("{}/ados-supervisor", env::BIN_DIR),
            "/usr/local/bin/ados-supervisor",
        ),
    ];
    for (target, link) in pairs {
        // `ln -sf` overwrites an existing link idempotently.
        if !crate::exec::run_ok("ln", &["-sf", &target, link]) {
            tracing::warn!(target = %target, link, "global symlink install failed");
        }
    }
}

/// Prebuilt-binary fetch + global symlink install.
pub struct FetchBinaries;

impl Step for FetchBinaries {
    fn id(&self) -> &str {
        "fetch_binaries"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("global-symlinks")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Prebuilt assets target aarch64 only. On a non-aarch64 dev host there
        // is nothing to fetch; skip cleanly (the bash path does the same).
        if !ctx.env.supported_arch {
            tracing::warn!(
                arch = %ctx.env.arch,
                "no prebuilt binaries for this arch; skipping fetch"
            );
            return StepOutcome::Skipped;
        }

        let channel = channel_of(ctx);
        let tmp_dir: PathBuf = match tempdir() {
            Ok(d) => d,
            Err(e) => return StepOutcome::Failed(format!("could not create temp dir: {e}")),
        };

        // The device-tree model keys the vision-binary variant (a CPU-ONNX board
        // fetches the onnx-enabled vision build). Read once for the whole fetch.
        let board_model = crate::steps::npu_provision::read_board_model();

        // Drive the determinate "Downloading components" bar: k of N binaries.
        let sink = ctx.progress.clone();
        let bins = binaries::for_profile(&ctx.profile);
        let total = bins.len() as u64;
        sink.sub_progress(self.id(), 0, total);
        for (i, b) in bins.into_iter().enumerate() {
            sink.activity(self.id(), format!("installing {}", b.service));
            let ok = match install_service(b, &board_model, &tmp_dir, channel, &sink) {
                Ok(()) => {
                    // Kept at debug: the live-detail pane names each component as
                    // it lands, so an info line here would just repeat "installed
                    // prebuilt binary" N times in the scroll-back. The journal
                    // still records it.
                    tracing::debug!(
                        service = b.service,
                        dest = b.dest,
                        "installed prebuilt binary"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(service = b.service, error = %e, "prebuilt binary fetch/verify failed after retries");
                    false
                }
            };
            // A Hard-gate miss aborts the install BEFORE systemd runs.
            if gate_outcome(b.gate, ok) == Decision::FailRequired {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(format!(
                    "required prebuilt binary {} could not be installed",
                    b.service
                ));
            }
            sink.sub_progress(self.id(), (i as u64) + 1, total);
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);

        // All Hard gates satisfied → install the global symlinks.
        install_global_symlinks();
        StepOutcome::Ok
    }
}

/// Create a unique temp directory under the system temp root for this run's
/// downloads. We roll our own (instead of pulling `tempfile` into the non-dev
/// build) using the pid + a monotonic counter.
fn tempdir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("ados-installer-fetch-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::PREBUILT;

    #[test]
    fn each_hard_gate_failing_means_fail_required() {
        // The MAVLink router is the sole C2 path with no Python fallback, so it
        // is a Hard gate alongside the orchestrator/video/cloud/vision set.
        for svc in [
            "ados-supervisor",
            "ados-mavlink-router",
            "ados-video",
            "ados-cloud",
            "ados-vision",
        ] {
            let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
            assert_eq!(b.gate, Gate::Hard, "{svc} must be a Hard gate");
            assert_eq!(
                gate_outcome(b.gate, false),
                Decision::FailRequired,
                "{svc} failing must abort the install"
            );
            // A Hard gate succeeding still continues.
            assert_eq!(gate_outcome(b.gate, true), Decision::Continue);
        }
    }

    #[test]
    fn best_effort_failing_continues() {
        // Pick a couple of best-effort catalog entries.
        for svc in ["ados-tui", "ados-radio", "ados-groundlink"] {
            let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
            assert_eq!(b.gate, Gate::BestEffort);
            assert_eq!(
                gate_outcome(b.gate, false),
                Decision::Continue,
                "{svc} (best-effort) failing must NOT abort the install"
            );
        }
    }

    #[test]
    fn channel_and_allow_unsigned_pairing() {
        // edge tolerates unsigned; stable does not.
        assert!(allow_unsigned_for(Channel::Edge));
        assert!(!allow_unsigned_for(Channel::Stable));
    }
}
