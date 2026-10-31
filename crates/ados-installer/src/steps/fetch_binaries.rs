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
//!
//! # Installing a locally-built binary (`--artifacts <dir>`)
//!
//! A bench node validating agent code that has not landed cannot fetch it: the
//! release host only carries what CI published. `--artifacts <dir>` makes that
//! directory the byte source for every catalog entry it carries, and leaves the
//! rest on the release. It is not a second install path — the bytes go through
//! the same [`install_one`] sequence a fetched asset does, and the table on that
//! function states exactly which checks apply to each source.
//!
//! On the build host, next to the binaries (an optional `manifest.json` in the
//! `prebuilt-main` shape, keyed by these file names, is checked too):
//!
//! ```text
//! cargo build --release -p ados-video
//! install -m 0755 target/release/ados-video /tmp/stage/ados-video
//! (cd /tmp/stage && sha256sum ados-video > ados-video.sha256)
//! ```
//!
//! On the node (the `.sha256` is mandatory; it is what makes the copy verifiable
//! rather than trusted):
//!
//! ```text
//! sudo ados-installer --upgrade --profile drone --channel edge \
//!     --artifacts /tmp/stage
//! ```
//!
//! `scripts/install.sh` forwards the flag verbatim on Linux, so
//! `scripts/install.sh --upgrade --artifacts /tmp/stage` works the same way —
//! with the caveat that the bootstrap fetches the RELEASED `ados-installer`
//! asset, so a node validating an unlanded change to the installer itself has to
//! run the locally-built `ados-installer` binary directly, as above.

use std::path::{Path, PathBuf};

use crate::binaries::{self, Gate, PrebuiltBinary};
use crate::ctx::Ctx;
use crate::env;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;
use crate::verify::{self, SignaturePolicy};
use ados_term::fmt::fmt_bytes;
use ados_term::progress::ProgressSink;

mod manifest;

pub use manifest::{Manifest, ReleaseRef, MANIFEST_ASSET};

mod resolve;
pub(crate) use resolve::release_base;
use resolve::*;
pub use resolve::{
    pinned_release, rev_release_missing, rev_release_tag, version_release_missing,
    version_release_tag, RELEASE_BASE_ENV,
};
mod download;
use download::*;
pub use download::{local_artifact, AssetSource};
mod artifact_check;
pub use artifact_check::ignored_artifact_names;
use artifact_check::*;
mod place;
pub(crate) use place::global_symlinks;
pub use place::prev_sibling;
use place::*;

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

// ---------------------------------------------------------------------------
// Where one catalog entry's bytes come from.
// ---------------------------------------------------------------------------

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
        // Linux supports aarch64 and x86_64 (preflight). Any other arch has no
        // binaries at all; skip cleanly (the bash path refuses it earlier).
        if !ctx.env.supported_arch {
            tracing::warn!(
                arch = %ctx.env.arch,
                "no prebuilt binaries for this arch; skipping fetch"
            );
            return StepOutcome::Skipped;
        }

        // The release publishes aarch64 binaries only. Any other supported arch
        // (x86_64) installs entirely from a locally built `--artifacts` dir, and
        // an entry missing from it is never fetched from the release (it would
        // land an aarch64 binary that cannot run here).
        let release_assets = env::arch_has_release_assets(&ctx.env.arch);
        if !release_assets && ctx.artifacts.is_none() {
            return StepOutcome::Failed(no_release_assets(&ctx.env.arch));
        }

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

        // Every release-sourced asset of one install comes from one release:
        // `rev-<sha>` for `--ref`, `v<X.Y.Z>` for stable, and on unpinned edge
        // the `rev-<sha>` the signed `prebuilt-main` manifest names. A `--ref`
        // pin is read from `ctx.rev`, which `venv_agent` has already expanded
        // to a full object name.
        let rev = ctx.rev.clone();
        if let Some(rev) = rev.as_deref() {
            if !is_full_object_name(rev) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(rev_not_expanded(rev));
            }
        }
        let pin = match pinned_release(&ctx.channel, rev.as_deref(), ctx.args.version.as_deref()) {
            Ok(pin) => pin,
            Err(msg) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(msg);
            }
        };

        // `--artifacts <dir>`: validate the directory BEFORE anything touches
        // the network — a request that cannot mean what the operator meant
        // should cost nothing and say why.
        let artifacts = ctx.artifacts.clone();
        let mut ignored: Vec<String> = Vec::new();
        if let Some(dir) = artifacts.as_deref() {
            match validate_artifacts_dir(dir) {
                Ok(names) => ignored = names,
                Err(msg) => {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return StepOutcome::Failed(msg);
                }
            }
        }

        // Resolve the release only when some entry will be fetched from it.
        // A pinned release is probed BEFORE the loop: without this the first
        // Hard-gate binary aborts with a bare "could not be installed", which
        // reads as a broken download rather than as the far likelier "that
        // release never published binaries".
        let needs_release = bins.iter().any(|b| {
            artifacts
                .as_deref()
                .is_none_or(|d| local_artifact(d, b).is_none())
        });
        let release: Option<ReleaseRef> = if release_assets && needs_release {
            sink.activity(
                self.id(),
                match pin.as_deref() {
                    Some(tag) => format!("checking the {tag} release"),
                    None => format!("reading the {} manifest", manifest::POINTER_TAG),
                },
            );
            let release = match resolve_release(pin.as_deref(), &tmp_dir) {
                Ok(r) => r,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return StepOutcome::Failed(format!(
                        "{e}. An edge install fetches its binaries from the release this \
                         manifest names; pin one with --ref <sha> if the pointer is unreachable."
                    ));
                }
            };
            if let Some(m) = release.manifest() {
                sink.sub_log(
                    self.id(),
                    &format!("edge build {} (agent {})", release.tag(), m.agent_version),
                );
            } else {
                let sample = bins
                    .iter()
                    .find(|b| b.gate == Gate::Hard)
                    .copied()
                    .unwrap_or(binaries::default_vision_binary());
                if !pinned_release_published(&release, sample, &tmp_dir) {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return StepOutcome::Failed(match rev.as_deref() {
                        Some(rev) => rev_release_missing(rev),
                        None => version_release_missing(release.tag()),
                    });
                }
            }
            Some(release)
        } else {
            None
        };

        // Resolve each entry's byte source once, and SAY which ones came from
        // the local directory and which files were passed over. A fallback to the
        // release is legitimate (a one-crate rebuild) but it must never be
        // silent: these lines are what tell the operator that the binary they are
        // about to measure is theirs, and what makes a mistyped filename visible.
        let host_arch = ctx.env.arch.clone();
        let sources: Vec<AssetSource<'_>> = bins
            .iter()
            .map(|b| source_for(b, artifacts.as_deref(), &host_arch, release.as_ref()))
            .collect();
        if let Some(dir) = artifacts.as_deref() {
            let local: Vec<&str> = bins
                .iter()
                .zip(&sources)
                .filter(|(_, s)| matches!(s, AssetSource::Local { .. }))
                .map(|(b, _)| b.service)
                .collect();
            let fetched = bins.len() - local.len();
            sink.sub_log(
                self.id(),
                &format!(
                    "local artifacts: {} ({fetched} from the release)",
                    local.join(", ")
                ),
            );
            if !ignored.is_empty() {
                sink.sub_log(
                    self.id(),
                    &format!("not a service binary, passed over: {}", ignored.join(", ")),
                );
            }
            tracing::info!(
                local = %local.join(","),
                fetched,
                ignored = %ignored.join(","),
                dir = %dir.display(),
                "installing locally-built service binaries"
            );
        }

        let total = bins.len() as u64;
        sink.sub_progress(self.id(), 0, total);
        for (i, (b, source)) in bins.iter().zip(&sources).enumerate() {
            sink.activity(self.id(), format!("installing {}", b.service));
            let result = if !release_assets && matches!(source, AssetSource::Release { .. }) {
                Err(anyhow::anyhow!(
                    "not in the --artifacts directory, and no {} release asset exists",
                    ctx.env.arch
                ))
            } else {
                install_service(b, &board_model, &tmp_dir, &sink, source)
            };
            let ok = match result {
                Ok(replaced) => {
                    // Recorded as each lands, so a later Hard-gate miss in this
                    // same loop still rolls back what was already swapped.
                    ctx.replaced_binaries.extend(replaced);
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

/// Create this run's download directory under the system temp root: a fresh,
/// randomly named directory created exclusively with mode 0700, so nothing
/// another local user pre-created can stand in for a verified binary. The
/// caller removes it when the step ends.
fn tempdir() -> std::io::Result<PathBuf> {
    Ok(tempfile::Builder::new()
        .prefix("ados-installer-fetch-")
        .tempdir()?
        .keep())
}

#[cfg(test)]
mod tests;
