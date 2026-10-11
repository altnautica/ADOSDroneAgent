//! Asset staging and per-service download with retry.

use super::*;

/// The byte source for one catalog entry.
///
/// This is a source of BYTES, not a choice of install path: both variants hand
/// the same three staged files (`<dest>.dl`, `.dl.sha256`, optional
/// `.dl.minisig`) to the one verify → chmod → atomic-replace → `.prev`
/// retention sequence in [`install_one`]. Nothing downstream of
/// [`stage_asset`] knows which variant produced them, so a local artifact
/// cannot skip a check by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetSource<'a> {
    /// The GitHub release host: the one release this run resolved (`rev-<sha>`
    /// from the edge manifest or `--ref`, `v<X.Y.Z>` for stable).
    Release {
        /// The resolved release; `None` on a host the release publishes nothing
        /// for (x86_64), where a release-sourced entry is refused unfetched.
        release: Option<&'a ReleaseRef>,
        /// The host architecture the fetched binary has to run on, checked
        /// against the downloaded ELF header before it is placed.
        host_arch: &'a str,
    },
    /// A directory of locally-built artifacts (`--artifacts <dir>`).
    Local {
        /// The directory holding `<service>` (or `<asset>`) plus its `.sha256`.
        dir: &'a Path,
        /// The host architecture the placed binary has to run on, checked
        /// against the artifact's ELF header before it is placed.
        host_arch: &'a str,
    },
}

impl AssetSource<'_> {
    /// How many times a staging failure is worth retrying.
    ///
    /// Three for a release fetch, because the failure it recovers from is a
    /// transient link drop (the field failure on a flaky USB WiFi). One for a
    /// local directory: a missing file or a bad digest is not going to fix
    /// itself, and sleeping 3 s before saying so only delays the message.
    pub(super) fn max_attempts(&self) -> u32 {
        match self {
            AssetSource::Release { .. } => 3,
            AssetSource::Local { .. } => 1,
        }
    }

    /// How strictly this source's signature is judged.
    ///
    /// A release download needs a valid signature on every channel: its
    /// `.sha256` comes from the same host as the binary, so on its own it proves
    /// the transfer and not the origin. Only a locally-built artifact, which
    /// cannot carry the CI signature (the key is a CI secret), may install on
    /// the SHA256 its build host recorded.
    pub(super) fn signature_policy(&self) -> SignaturePolicy {
        match self {
            AssetSource::Release { .. } => SignaturePolicy::Required,
            AssetSource::Local { .. } => SignaturePolicy::LocalBuild,
        }
    }
}

/// The local file backing `b`, if the artifacts directory carries one.
///
/// Two accepted spellings, in priority order: the release asset name
/// (`ados-video-aarch64`, i.e. a downloaded asset dropped into the directory)
/// and the plain service name (`ados-video`, i.e. what `cargo build --release`
/// leaves in `target/<triple>/release`). Nothing else is guessed — a name close
/// to but not equal to one of those is reported by
/// [`unrecognised_artifact_names`] rather than silently ignored.
pub fn local_artifact(dir: &Path, b: &PrebuiltBinary) -> Option<PathBuf> {
    for name in [b.asset, b.service] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The byte source for `b`: the local directory when it carries the artifact,
/// the release otherwise.
///
/// The per-entry fallback is deliberate and is what makes the flag usable: a
/// bench validating one unlanded crate copies one binary, and the other fourteen
/// services still come from the release they would have come from anyway. The
/// step reports the split so the fallback is never silent.
pub(super) fn source_for<'a>(
    b: &PrebuiltBinary,
    artifacts: Option<&'a Path>,
    host_arch: &'a str,
    release: Option<&'a ReleaseRef>,
) -> AssetSource<'a> {
    match artifacts {
        Some(dir) if local_artifact(dir, b).is_some() => AssetSource::Local { dir, host_arch },
        _ => AssetSource::Release { release, host_arch },
    }
}

/// The failure text when a local artifact has no `.sha256` beside it.
///
/// The installer does NOT hash the file itself and call that verified: a digest
/// computed from the same bytes it is checking proves nothing. The sidecar is
/// written on the build host, so comparing against it is a real check — it
/// catches a truncated `scp`, a half-written file, and the wrong binary copied
/// under the right name. Refusing loudly with the exact command to produce it is
/// the only honest option; skipping it would leave the local path claiming a
/// verification it did not perform.
pub(super) fn local_sha_missing(service: &str, artifact: &Path) -> String {
    format!(
        "{service}: {} has no .sha256 beside it. A local artifact is verified \
         against the digest its BUILD host recorded, so the installer will not \
         compute one from the bytes it is checking. On the build host run: \
         sha256sum {0} > {0}.sha256 (macOS: shasum -a 256), and copy both files.",
        artifact.display()
    )
}

/// Stage one catalog entry's bytes (plus its `.sha256`, plus its `.minisig` when
/// one exists) at the `.dl` paths [`install_one`] verifies and places.
///
/// The two arms differ only in where the bytes come from. Everything that
/// decides whether they are installable happens after this returns.
pub(super) fn stage_asset(
    b: &PrebuiltBinary,
    source: &AssetSource<'_>,
    dl_bin: &Path,
    dl_sha: &Path,
    dl_sig: &Path,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    match *source {
        AssetSource::Release { release, host_arch } => {
            let release = release.ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: no release resolved to fetch {} from",
                    b.service,
                    b.asset
                )
            })?;
            let asset_url = release.asset_url(b.asset);
            // Stream byte progress so the live pane shows "<service> 4.2/8.1 MB".
            net::fetch_with_progress(&asset_url, dl_bin, |done, total| {
                sink.byte_progress("fetch_binaries", done, total, b.service);
            })?;
            // Refuse a release asset this host cannot run (the release only
            // publishes aarch64) before it replaces a working binary.
            if let Some(why) = artifact_arch_error(&read_header(dl_bin), host_arch) {
                anyhow::bail!("{}: release asset {} {why}", b.service, b.asset);
            }
            match release.manifest() {
                // The signed manifest is the digest of record for an edge
                // build: the verifier checks the bytes against it, not against
                // a sidecar served beside the asset.
                Some(m) => {
                    let digest = m.check_size(b.asset, dl_bin)?;
                    manifest::write_digest_sidecar(digest, b.asset, dl_sha)?;
                }
                None => net::fetch(&format!("{asset_url}.sha256"), dl_sha)?,
            }
            // The signature is mandatory for a release asset, but its fetch is
            // allowed to fail here so the refusal comes from the verifier, which
            // names the artifact and the reason. curl is invoked with `-f`, so a
            // 404 leaves no file rather than a saved error page.
            let _ = net::fetch(&format!("{asset_url}.minisig"), dl_sig);
            Ok(())
        }
        AssetSource::Local { dir, host_arch } => {
            let src = local_artifact(dir, b).ok_or_else(|| {
                anyhow::anyhow!("{}: no artifact in {}", b.service, dir.display())
            })?;
            let src_sha = sidecar_path(&src, "sha256");
            if !src_sha.is_file() {
                anyhow::bail!(local_sha_missing(b.service, &src));
            }
            // Refuse a binary this host cannot run BEFORE it replaces a working
            // one. A local directory is exactly where a Mach-O build from the
            // developer's laptop, or a build for the wrong target from the
            // wrong target dir, gets picked up. Cheap and side-effect-free: 20
            // bytes of header, no exec.
            if let Some(why) = artifact_arch_error(&read_header(&src), host_arch) {
                anyhow::bail!("{}: {} {why}", b.service, src.display());
            }
            std::fs::copy(&src, dl_bin)
                .map_err(|e| anyhow::anyhow!("copy {} failed: {e}", src.display()))?;
            std::fs::copy(&src_sha, dl_sha)
                .map_err(|e| anyhow::anyhow!("copy {} failed: {e}", src_sha.display()))?;
            // A bench manifest beside the binaries pins their digests too: the
            // bytes must match it, not only the build host's sidecar.
            if let Some(m) = manifest::local_manifest(dir)? {
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let digest = m.check_size(&name, dl_bin)?;
                manifest::write_digest_sidecar(digest, &name, dl_sha)?;
            }
            let src_sig = sidecar_path(&src, "minisig");
            if src_sig.is_file() {
                let _ = std::fs::copy(&src_sig, dl_sig);
            }
            let size = std::fs::metadata(dl_bin).map(|m| m.len()).unwrap_or(0);
            sink.byte_progress("fetch_binaries", size, size, b.service);
            Ok(())
        }
    }
}

/// Obtain + verify one prebuilt binary, then place it atomically at the
/// destination the catalog names for it.
///
/// `source` decides only where the bytes come from — a release asset, or a file
/// from the `--artifacts` directory (see [`stage_asset`]). Everything after the
/// staging call is identical for both, which is the point: a local artifact is a
/// byte source, never a shortcut past the checks. What the two sources actually
/// get, in order:
///
/// | check | release asset | local artifact |
/// |---|---|---|
/// | ELF machine matches this host | applied | **applied** |
/// | SHA256 against the `.sha256` sidecar | applied (sidecar fetched) | **applied** (sidecar copied from the build host; a missing one is fatal) |
/// | minisign against the vendored trust anchor | **mandatory**: a missing `.minisig` or `minisign` is fatal on every channel | applied when a `.minisig` exists; a missing one warns (the signing key is a CI secret) |
/// | chmod 0755, atomic rename, `<dest>.prev` retention, Hard/BestEffort gate | applied | applied |
pub(super) fn install_one(
    b: &PrebuiltBinary,
    tmp_dir: &Path,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<Option<PathBuf>> {
    let dest = Path::new(b.dest);
    let replaced = install_one_at(b, dest, tmp_dir, sink, source)?;
    Ok(replaced.then(|| dest.to_path_buf()))
}

/// Obtain + verify + place one binary at `dest`. Returns whether an existing
/// binary was replaced on success, `Err` on any stage/verify/place miss (the
/// caller maps that through the gate).
///
/// `dest` is a parameter rather than read off `b` because placement is a
/// property of this call, not of the catalog: the sequence is "stage these
/// bytes, verify them, swap them over THIS path, keep the outgoing copy". Every
/// production caller passes `b.dest` through [`install_one`]; a test passes a
/// tempdir, which is what makes the verify-and-rollback ordering assertable
/// without a writable `/opt/ados/bin`.
///
/// `tmp_dir` holds nothing for the binary itself — the bytes are staged at a
/// `.dl` sibling of `dest` so the final placement is a same-filesystem `rename`
/// (see [`place_binary`]); the dir is retained for callers that want a scratch
/// root and for symmetry.
pub(super) fn install_one_at(
    b: &PrebuiltBinary,
    dest: &Path,
    _tmp_dir: &Path,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<bool> {
    // Ensure /opt/ados/bin exists so the `.dl` sibling and the final rename land
    // on the same filesystem as the destination (atomic rename requires it).
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {} failed: {e}", parent.display()))?;
    }

    // Stage the binary + its sidecars as siblings of the real dest. The `.sha256`
    // MUST sit next to the binary we verify because `verify_artifact` looks for
    // `<artifact>.sha256` beside the artifact.
    let dl_bin = dl_sibling(dest);
    let dl_sha = sidecar_path(&dl_bin, "sha256");
    let dl_sig = sidecar_path(&dl_bin, "minisig");

    let outcome = (|| {
        stage_asset(b, source, &dl_bin, &dl_sha, &dl_sig, sink)?;

        // Verify the staged temp BEFORE it is placed at the live path. A release
        // asset needs its signature; a local build may lack one. The `.sha256`
        // is NOT best-effort on either arm: the fetch fails on a missing one,
        // and the local arm refuses before it copies anything.
        verify::verify_artifact(&dl_bin, verify::RELEASE_PUBKEY, source.signature_policy())?;

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
        place_binary(&dl_bin, dest)
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

/// Obtain + verify + place one binary, retrying on failure with exponential
/// backoff. A single attempt's curl `--retry` (with `--continue-at -` resume)
/// already recovers a short drop mid-transfer; this outer loop adds spaced
/// retries so a longer management-link outage during one binary does not doom
/// the whole install (the field failure on a flaky USB WiFi where one of ~15
/// binaries dropped and aborted the install). Bounded so a genuinely
/// unreachable asset still fails instead of stalling forever.
///
/// A local artifact gets ONE attempt ([`AssetSource::max_attempts`]): its
/// failures — no file, no `.sha256`, wrong architecture, bad digest — are all
/// terminal, and retrying them only puts 3 s between the operator and the
/// message that says what to fix.
pub(super) fn install_one_with_retry(
    b: &PrebuiltBinary,
    tmp_dir: &Path,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<Option<PathBuf>> {
    let max_attempts = source.max_attempts();
    let mut backoff = std::time::Duration::from_secs(1);
    for attempt in 1..=max_attempts {
        match install_one(b, tmp_dir, sink, source) {
            Ok(replaced) => return Ok(replaced),
            Err(e) if attempt < max_attempts => {
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
/// variant (the default build still installs and honestly reports no
/// real inference until the onnx variant is available). Every other service
/// installs its single catalog binary unchanged.
///
/// The variant selection is a RELEASE-side choice: it picks between two
/// published assets. An operator who hands `ados-vision` to `--artifacts` has
/// already chosen which build they want, so the local arm installs that file and
/// does not go looking for an onnx variant to prefer over it.
pub(super) fn install_service(
    b: &PrebuiltBinary,
    board_model: &str,
    tmp_dir: &Path,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<Vec<PathBuf>> {
    if matches!(source, AssetSource::Release { .. })
        && b.service == "ados-vision"
        && binaries::board_prefers_onnx_vision(board_model)
    {
        // The onnx binary links the ONNX Runtime dynamically, so the binary AND
        // its shared library are installed together — either both land or the
        // install falls back to the default (musl, no-onnx) build. Installing the
        // onnx binary without its runtime would leave a vision service that
        // cannot dlopen ORT at start.
        match install_onnx_vision(tmp_dir, sink, source) {
            Ok(replaced) => return Ok(replaced),
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
    Ok(install_one_with_retry(b, tmp_dir, sink, source)?
        .into_iter()
        .collect())
}
