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
//! On the build host, next to the binaries:
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
use crate::ui::{activity, ProgressSink};
use crate::verify::{self, Channel};

/// GitHub release-download base; each prebuilt asset hangs off
/// `<base>/<release_tag>/<asset>` (plus `.sha256` / `.minisig` sidecars).
const RELEASE_BASE: &str = "https://github.com/altnautica/ADOSDroneAgent/releases/download";

/// Environment override for [`RELEASE_BASE`], for tests only.
///
/// The whole fetch stack is anonymous `curl`, which speaks `file://`, so a fake release
/// tree on disk gives the pinned + unpinned resolution a full end-to-end exercise with
/// no network and no credential. It follows the driver layer's `ADOS_PREBUILT_BASE_URL`
/// (`scripts/drivers/lib-prebuilt.sh`), including the part that matters: an operator
/// never sets it, and nothing in the install writes it.
pub const RELEASE_BASE_ENV: &str = "ADOS_RELEASE_BASE";

/// The release-download base in force for this run.
pub(crate) fn release_base() -> String {
    base_or_default(std::env::var(RELEASE_BASE_ENV).ok().as_deref())
}

/// Pick the base from an override value (pure). A blank or whitespace-only
/// override is treated as absent: an exported-but-empty variable must not turn
/// every asset URL into a relative path curl would refuse.
fn base_or_default(override_value: Option<&str>) -> String {
    match override_value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => RELEASE_BASE.to_string(),
    }
}

/// The per-revision release tag CI publishes for one commit.
///
/// `rev-<full 40-char sha>`. The tag is derived in-shell inside the workflow
/// rather than declared as a job variable, because a templated tag is exactly
/// what the installer's release-tag guard refuses to accept.
pub fn rev_release_tag(rev: &str) -> String {
    format!("rev-{rev}")
}

/// Whether `rev` is the full 40-character object name CI names a release after.
///
/// The tag is `rev-<full sha>`, so a prefix cannot address it: `venv_agent`
/// expands one from the clone's own object store before this step runs. This is
/// the assertion of that contract rather than a fallback for it.
fn is_full_object_name(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

/// The failure text when a pin reaches the fetch still abbreviated.
///
/// Reachable in one narrow case: a RESUMED fresh install whose `venv`
/// checkpoint is already marked skips `venv_agent`, so nothing expanded the
/// prefix. Worth its own message, because the per-revision-release message
/// below would blame the workflow's path filter for what is really a skipped
/// step.
fn rev_not_expanded(rev: &str) -> String {
    format!(
        "--ref {rev} is not a full 40-character commit, and the step that expands it \
         (the source checkout) did not run this time — a resumed install skips it once its \
         `venv` checkpoint is marked. Re-run with --force to redo the checkout, or pass the \
         full 40-character SHA so the per-revision release can be addressed directly."
    )
}

/// The version release a stable install resolves every asset from.
///
/// `v<X.Y.Z>`: the release workflow mirrors the tagged commit's service
/// binaries into the same release as the wheel and the deploy bundle, so the
/// stable channel places exactly the binaries that were built from the source it
/// installs. `version` is the bare form (`venv_agent::normalize_version`).
pub fn version_release_tag(version: &str) -> String {
    format!("v{version}")
}

/// The release a pinned install resolves every asset from (pure), or `None` for
/// the rolling per-service tags.
///
/// * `stable` is always pinned: to `v<version>`, and it refuses to run without
///   a `--version`. The rolling tags are whatever `main` last built, which is
///   exactly what a stable install must never place.
/// * Every other channel is pinned only by `--ref`, to `rev-<sha>`.
pub fn pinned_release(
    channel: &str,
    rev: Option<&str>,
    version: Option<&str>,
) -> Result<Option<String>, String> {
    if channel == "stable" {
        let version = version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                "the stable channel installs the service binaries of one pinned release and \
             requires --version (the release to install); refusing to fall back to the \
             rolling per-service tags"
                    .to_string()
            })?;
        return Ok(Some(version_release_tag(
            &crate::steps::venv_agent::normalize_version(version),
        )));
    }
    Ok(rev.map(rev_release_tag))
}

/// Where one asset's URL hangs off: the rolling per-service tag normally, or the
/// pinned release (`rev-<sha>` for `--ref`, `v<X.Y.Z>` for stable).
///
/// This is the SOLE place a prebuilt asset URL gets its base, so a pin cannot
/// half-apply: every binary in the catalog, the onnx vision variant, and the
/// ONNX Runtime library all resolve through here. `pin` is a complete release
/// tag from [`pinned_release`]; a `--ref` inside it is already the full object
/// name — `venv_agent` expands an abbreviated `--ref` from the clone before this
/// step runs, so no prefix is ever handed to a server that cannot resolve one.
pub fn asset_base(pin: Option<&str>, tag: &str) -> String {
    match pin {
        Some(pin) => format!("{}/{pin}", release_base()),
        None => format!("{}/{tag}", release_base()),
    }
}

/// The failure text when a pinned install finds no per-revision release.
///
/// It names the path filter, because that is the likely reason and it is not
/// guessable from a 404: `.github/workflows/rust.yml` only runs on changes under
/// `crates/**`, the four generated `.py` contract files, `data/systemd/**`, and
/// itself. A commit touching only `src/ados/**` therefore publishes nothing,
/// and the alternative — dropping the filter — pays ~23 arm64 release builds for
/// every commit to the Python tree.
pub fn rev_release_missing(rev: &str) -> String {
    format!(
        "no {tag} release exists, so the prebuilt binaries for revision {rev} were never \
         published. The workflow that builds them runs only on commits touching `crates/**`, \
         the generated `_*_generated.py` contract files, or `data/systemd/**` — a commit that \
         changed only `src/ados/**` publishes no per-revision release. Pin a commit that \
         touched the Rust tree, or re-run without --ref to install from the rolling \
         per-service release tags.",
        tag = rev_release_tag(rev)
    )
}

/// The failure text when a stable install finds no service binaries in its
/// version release.
pub fn version_release_missing(tag: &str) -> String {
    format!(
        "the {tag} release carries no prebuilt service binaries, so the stable channel has \
         nothing pinned to install. Stable places only the binaries published with its own \
         release; it does not fall back to the rolling per-service tags. Install a release \
         that carries its binaries, or use --channel edge."
    )
}

/// Whether the pinned release actually carries assets.
///
/// One sidecar fetch answers it: `<base>/<pin>/<asset>.sha256` exists only if CI
/// published the binaries into that release. `sample` is a Hard-gated catalog
/// entry, so a present sidecar means the release holds the assets the install
/// cannot do without — not merely that a tag of that name exists. Cheap enough to
/// pay for on every pinned install (a few hundred bytes) and it converts an
/// opaque per-binary 404 into the one message that names the cause.
fn pinned_release_published(pin: &str, sample: &PrebuiltBinary, tmp_dir: &Path) -> bool {
    let url = format!(
        "{}/{}.sha256",
        asset_base(Some(pin), sample.release_tag),
        sample.asset
    );
    let probe = tmp_dir.join("pinned-release-probe.sha256");
    let ok = net::fetch(&url, &probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

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

/// Resolve the channel enum from the ctx's channel string. The lenient branch is
/// opt-in by exact name; see [`Channel::from_name`].
fn channel_of(ctx: &Ctx) -> Channel {
    Channel::from_name(&ctx.channel)
}

/// Whether a prebuilt fetch may skip the signature check outright.
///
/// Never, on any channel — which is why the channel is not consulted.
///
/// `allow_unsigned` short-circuits inside [`verify::verify_artifact`] BEFORE the
/// pubkey is read, so it does not mean "tolerate a missing signature"; it means
/// "do not look at signatures at all". Passing it on the default channel meant
/// the vendored trust anchor below was never consulted on the path almost every
/// install takes, so a binary carrying a signature that does NOT match it was
/// installed without complaint. The key has been embedded here since it was
/// generated, and this flag is the reason none of it ever ran.
///
/// Tolerating a signature we cannot OBTAIN is a separate and much weaker
/// decision, and it already has a home that is still channel-gated:
/// `verify_minisign` routes a missing `.minisig`, or a host with no `minisign`
/// binary, through `unverifiable`, which warns on edge and refuses on stable.
/// That is what keeps today's unsigned releases installable, so turning this off
/// changes nothing for an install fetching an unsigned asset — and refuses a
/// tampered one, everywhere, which is the case that mattered.
fn allow_unsigned_for(_channel: Channel) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Where one catalog entry's bytes come from.
// ---------------------------------------------------------------------------

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
    /// The GitHub release host: the rolling per-service tag, or the pinned
    /// release (`rev-<sha>` for `--ref`, `v<X.Y.Z>` for stable).
    Release {
        /// The pinned release tag, from [`pinned_release`].
        pin: Option<&'a str>,
    },
    /// A directory of locally-built artifacts (`--artifacts <dir>`).
    Local {
        /// The directory holding `<service>` (or `<asset>`) plus its `.sha256`.
        dir: &'a Path,
        /// The host architecture the placed binary has to run on, used for the
        /// ELF gate the release path does not need (CI only publishes aarch64).
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
    fn max_attempts(&self) -> u32 {
        match self {
            AssetSource::Release { .. } => 3,
            AssetSource::Local { .. } => 1,
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
fn source_for<'a>(
    b: &PrebuiltBinary,
    artifacts: Option<&'a Path>,
    host_arch: &'a str,
    pin: Option<&'a str>,
) -> AssetSource<'a> {
    match artifacts {
        Some(dir) if local_artifact(dir, b).is_some() => AssetSource::Local { dir, host_arch },
        _ => AssetSource::Release { pin },
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
fn local_sha_missing(service: &str, artifact: &Path) -> String {
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
fn stage_asset(
    b: &PrebuiltBinary,
    source: &AssetSource<'_>,
    dl_bin: &Path,
    dl_sha: &Path,
    dl_sig: &Path,
    sink: &ProgressSink,
) -> anyhow::Result<()> {
    match *source {
        AssetSource::Release { pin } => {
            let asset_url = format!("{}/{}", asset_base(pin, b.release_tag), b.asset);
            // Stream byte progress so the live pane shows "<service> 4.2/8.1 MB".
            net::fetch_with_progress(&asset_url, dl_bin, |done, total| {
                sink.byte_progress("fetch_binaries", done, total, b.service);
            })?;
            net::fetch(&format!("{asset_url}.sha256"), dl_sha)?;
            // Best-effort: verification upgrades to signature-checked
            // automatically once CI signs. curl is invoked with `-f`, so a 404
            // leaves no file rather than a saved error page.
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
            // one. The release path does not need this (CI publishes aarch64
            // only); a local directory is exactly where a Mach-O build from the
            // developer's laptop, or an x86_64 build from the wrong target dir,
            // gets picked up. Cheap and side-effect-free: 20 bytes of header, no
            // exec.
            if let Some(why) = artifact_arch_error(&read_header(&src), host_arch) {
                anyhow::bail!("{}: {} {why}", b.service, src.display());
            }
            std::fs::copy(&src, dl_bin)
                .map_err(|e| anyhow::anyhow!("copy {} failed: {e}", src.display()))?;
            std::fs::copy(&src_sha, dl_sha)
                .map_err(|e| anyhow::anyhow!("copy {} failed: {e}", src_sha.display()))?;
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

/// The first 20 bytes of a file (an ELF identification block plus `e_machine`),
/// or an empty vector when it cannot be read. Short reads are fine: the parser
/// treats anything it cannot decode as not-an-ELF.
fn read_header(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut buf = [0u8; 20];
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    buf[..filled].to_vec()
}

/// The ELF `e_machine` value a binary must declare to run on `arch`, or `None`
/// for an architecture this check has no opinion about (in which case it does
/// not run at all — an unknown host is not grounds for refusing an artifact).
fn expected_elf_machine(arch: &str) -> Option<u16> {
    match arch {
        "aarch64" => Some(0xB7),
        "x86_64" => Some(0x3E),
        "riscv64" => Some(0xF3),
        _ => None,
    }
}

/// Why `header` cannot run on `host_arch` (pure), or `None` when it can — or
/// when this check cannot say.
///
/// Decodes the ELF identification block: magic, 64-bit class, endianness, and
/// the `e_machine` at offset 18. A file that is not an ELF at all is named as
/// such, because the likeliest way to reach that on a bench is copying the Mach-O
/// the same `cargo build` produced on the developer's Mac.
fn artifact_arch_error(header: &[u8], host_arch: &str) -> Option<String> {
    // A host architecture with no table entry: the check has no opinion and does
    // not run, rather than refusing an artifact it cannot judge.
    let want = expected_elf_machine(host_arch)?;
    if header.len() < 20 || &header[..4] != b"\x7fELF" {
        return Some(format!(
            "is not an ELF executable, so it cannot run on this {host_arch} host \
             (a macOS build of the same crate looks like this). Build it for the \
             node, e.g. cargo build --release --target aarch64-unknown-linux-gnu."
        ));
    }
    if header[4] != 2 {
        return Some("is a 32-bit ELF; this host runs 64-bit binaries.".to_string());
    }
    // EI_DATA: 1 = little-endian, 2 = big-endian. Every target here is LE, but
    // decode honestly rather than assume.
    let machine = match header[5] {
        1 => u16::from_le_bytes([header[18], header[19]]),
        2 => u16::from_be_bytes([header[18], header[19]]),
        _ => return Some("has an unreadable ELF data encoding.".to_string()),
    };
    if machine == want {
        return None;
    }
    Some(format!(
        "is built for ELF machine {machine:#x}, not the {host_arch} this node \
         runs ({want:#x}). Build it for the node, e.g. cargo build --release \
         --target aarch64-unknown-linux-gnu."
    ))
}

/// Names in an artifacts directory this install has no use for: not a sidecar,
/// not a catalog artifact.
///
/// Reported, never fatal, and deliberately so. The natural thing to point the
/// flag at is a `target/release` directory, which legitimately holds `*.d` dep
/// files, `lib*.rlib`, and `ados-installer` itself — none of which is a service
/// this installer places. Rejecting the directory for holding them would make
/// the flag unusable for its only purpose.
///
/// What the report buys is the case that actually matters: a mistyped filename
/// would otherwise be the worst outcome available here — that service silently
/// falls back to the release, the install succeeds, and the bench measures the
/// OLD binary believing it measured the new one. Naming both lists (what was
/// taken from the directory, what was passed over) puts the typo in front of the
/// operator, and [`validate_artifacts_dir`] still hard-fails the single-file
/// typo, where nothing at all is recognised.
pub fn ignored_artifact_names(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|n| !n.ends_with(".sha256") && !n.ends_with(".minisig"))
        .filter(|n| !is_catalog_artifact_name(n))
        .cloned()
        .collect()
}

/// Whether `name` is a spelling of some catalog artifact — any profile's, plus
/// the onnx vision variant and the ONNX Runtime library, since a directory
/// assembled for one profile may legitimately carry another's binary.
fn is_catalog_artifact_name(name: &str) -> bool {
    binaries::PREBUILT
        .iter()
        .chain([
            &binaries::PREBUILT_VISION_ONNX,
            &binaries::PREBUILT_VISION_ONNX_RUNTIME,
        ])
        .any(|b| b.asset == name || b.service == name)
}

/// The file names directly inside `dir` (no recursion), sorted.
fn dir_entry_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// Refuse an `--artifacts` directory that cannot mean what the operator meant,
/// and otherwise report what will be taken from it and what will be passed over.
///
/// Two refusals:
///
/// * The path is not a directory, or cannot be read — a typo, or a
///   `target/release` that was never built.
/// * It holds no catalog artifact at all. This is the single-file typo
///   (`ados-vidoe`) and the wrong-directory case, and it has to be fatal: the
///   alternative is an install that fetches every binary from the release and
///   reports success while the operator believes they installed their build.
///
/// Names it has no use for are returned, not refused — see
/// [`ignored_artifact_names`].
fn validate_artifacts_dir(dir: &Path) -> Result<Vec<String>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "--artifacts {}: not a directory. Point it at the directory holding \
             the built service binaries (e.g. target/release).",
            dir.display()
        ));
    }
    let names = dir_entry_names(dir)
        .map_err(|e| format!("--artifacts {}: cannot be read: {e}", dir.display()))?;
    if !names.iter().any(|n| is_catalog_artifact_name(n)) {
        return Err(format!(
            "--artifacts {}: holds no service binary. Each artifact is named for \
             its service (e.g. ados-video) or for its release asset (e.g. \
             ados-video-aarch64), with its .sha256 beside it. Nothing here \
             matches, so every binary would come from the release instead.",
            dir.display()
        ));
    }
    Ok(ignored_artifact_names(&names))
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
/// | ELF machine matches this host | not applied (CI publishes aarch64 only) | **applied** |
/// | SHA256 against the `.sha256` sidecar | applied (sidecar fetched) | **applied** (sidecar copied from the build host; a missing one is fatal) |
/// | minisign against the vendored trust anchor | applied when a `.minisig` exists | applied when a `.minisig` exists — in practice never, since the signing key is a CI secret |
/// | a signature that cannot be OBTAINED | warn on edge, fatal on stable | same rule; `--artifacts` is refused on stable up front for exactly this reason |
/// | chmod 0755, atomic rename, `<dest>.prev` retention, Hard/BestEffort gate | applied | applied |
fn install_one(
    b: &PrebuiltBinary,
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<()> {
    install_one_at(b, Path::new(b.dest), tmp_dir, channel, sink, source)
}

/// Obtain + verify + place one binary at `dest`. Returns `Ok(())` on success,
/// `Err` on any stage/verify/place miss (the caller maps that through the gate).
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
fn install_one_at(
    b: &PrebuiltBinary,
    dest: &Path,
    _tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<()> {
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

        // Verify the staged temp BEFORE it is placed at the live path. Every
        // channel checks any `.minisig` that arrived against the vendored trust
        // anchor; only whether a MISSING one is fatal still varies by channel.
        //
        // The best-effort `.minisig` staging above is what makes that safe to run
        // on the default channel today: curl is invoked with `-f`, so a 404
        // leaves no file at all rather than a saved error page, `net::fetch`
        // never promotes a failed transfer to the destination, and the local arm
        // copies a `.minisig` only when one exists. An absent sidecar therefore
        // reads as "unobtainable" (warn on edge) and not as a signature that
        // fails to verify. The `.sha256` is NOT best-effort on either arm: the
        // fetch fails on a missing one, and the local arm refuses before it
        // copies anything.
        verify::verify_artifact(
            &dl_bin,
            Some(verify::RELEASE_PUBKEY),
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
fn install_one_with_retry(
    b: &PrebuiltBinary,
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<()> {
    let max_attempts = source.max_attempts();
    let mut backoff = std::time::Duration::from_secs(1);
    for attempt in 1..=max_attempts {
        match install_one(b, tmp_dir, channel, sink, source) {
            Ok(()) => return Ok(()),
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
fn install_service(
    b: &PrebuiltBinary,
    board_model: &str,
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<()> {
    if let AssetSource::Release { pin } = *source {
        if b.service == "ados-vision" && binaries::board_prefers_onnx_vision(board_model) {
            // The onnx binary links the ONNX Runtime dynamically, so the binary AND
            // its shared library are installed together — either both land or the
            // install falls back to the default (musl, no-onnx) build. Installing the
            // onnx binary without its runtime would leave a vision service that
            // cannot dlopen ORT at start.
            match install_onnx_vision(tmp_dir, channel, sink, pin) {
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
    }
    install_one_with_retry(b, tmp_dir, channel, sink, source)
}

/// Whether a freshly-placed binary can actually `execve` on this host — probes
/// for the dynamic-linker rejection a glibc floor mismatch produces (the
/// CI-built onnx `ados-vision` links against whatever glibc its `ubuntu-22.04-arm`
/// build runner ships; a board running an older base image, e.g. Debian 11
/// Bullseye's glibc 2.31, rejects it at `execve` time with `GLIBC_x not found`
/// on stderr). That rejection happens before the program's own code runs, so a
/// short bounded wait distinguishes it cleanly from a binary that actually
/// starts: the loader either fails within milliseconds, or the process is
/// still alive when the deadline elapses (killed immediately — a fresh install
/// already tears down and restarts services around this point, so a killed
/// probe process is harmless). A spawn failure for any OTHER reason (missing
/// file, permission) is not what this probes for — treat it as "runs" so the
/// caller's existing error paths (the retry loop, the Hard/BestEffort gate)
/// handle it instead of this probe silently swallowing an unrelated fault.
fn binary_execs_on_this_host(path: &Path) -> bool {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long the loader is given to reach a verdict. A rejection happens
    /// before the program's own code runs, so this only has to cover process
    /// creation — but it is deliberately generous, because the cost of being
    /// too tight is not a slow probe, it is a WRONG one: a deadline that
    /// elapses during an ordinary fork on a loaded machine reads as "runs".
    const VERDICT_DEADLINE: Duration = Duration::from_secs(2);
    /// `ETXTBSY` on both Linux and macOS.
    const ETXTBSY: i32 = 26;
    const BUSY_RETRIES: u32 = 10;
    const BUSY_BACKOFF: Duration = Duration::from_millis(20);

    // A binary written moments ago can refuse to exec with ETXTBSY while any
    // descriptor still holds it open for writing — including one this process
    // never opened, since a concurrent fork inherits every open descriptor in
    // the program. That is a timing artefact, not a loader verdict, so retry it
    // rather than reading it as "runs" (which would skip the very check this
    // function exists to perform).
    let mut spawned: Option<Child> = None;
    for _ in 0..BUSY_RETRIES {
        match Command::new(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => {
                spawned = Some(c);
                break;
            }
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(BUSY_BACKOFF);
            }
            // Any other spawn failure is not what this probes for; leave it to
            // the caller's retry loop and Hard/BestEffort gate.
            Err(_) => return true,
        }
    }
    let Some(mut child) = spawned else {
        return true;
    };

    // Drain stderr on its own thread rather than after the process is reaped.
    // Reading only once `try_wait` reports an exit deadlocks whenever the
    // rejection message fills the pipe buffer: the child blocks on write, so it
    // never exits, so nothing ever drains it — and the probe then hits its
    // deadline and reports the rejected binary as runnable.
    let stderr_reader = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            // A read error keeps whatever arrived before it. An empty buffer
            // reads as "no rejection", which is the honest default: absence of
            // evidence, not evidence of rejection.
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + VERDICT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return true,
        }
    };

    // Either the child exited or we killed it; both close the pipe, so the
    // reader has finished and this join cannot hang.
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    match status {
        // Exited nonzero carrying a loader complaint: the rejection we probe for.
        Some(status) => !(!status.success() && stderr.contains("GLIBC_")),
        // Still alive at the deadline: the loader accepted it.
        None => true,
    }
}

/// Install the onnx-enabled `ados-vision` binary together with the ONNX Runtime
/// shared library it dlopens at start. Both must land, AND the binary must
/// actually execve on this host — if the runtime library cannot be fetched, or
/// the placed binary is rejected by the dynamic loader (a glibc floor
/// mismatch), this returns `Err` and the caller falls back to the default
/// vision build.
fn install_onnx_vision(
    tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    pin: Option<&str>,
) -> anyhow::Result<()> {
    // Both halves of the variant come from the release: this path is only
    // reached for a release-sourced `ados-vision` (see `install_service`).
    let source = AssetSource::Release { pin };
    install_one_with_retry(
        &binaries::PREBUILT_VISION_ONNX,
        tmp_dir,
        channel,
        sink,
        &source,
    )?;
    install_one_with_retry(
        &binaries::PREBUILT_VISION_ONNX_RUNTIME,
        tmp_dir,
        channel,
        sink,
        &source,
    )
    .map_err(|e| anyhow::anyhow!("ONNX Runtime library fetch failed: {e}"))?;

    if !binary_execs_on_this_host(Path::new(binaries::PREBUILT_VISION_ONNX.dest)) {
        anyhow::bail!(
            "onnx vision binary rejected by the dynamic loader on this host \
             (glibc floor mismatch — the board's base image is older than the \
             onnx build's glibc floor)"
        );
    }
    Ok(())
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
/// The retained previous copy of a placed binary: `<dest>.prev`.
///
/// Kept so a bad upgrade has somewhere to go back to. Until this existed the
/// only documented recovery was reinstalling from a specific git ref, which
/// needs a working shell, internet and a version the operator knows — none of
/// which a customer necessarily has once the upgrade that broke the box has
/// already landed.
pub fn prev_sibling(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

fn place_binary(src: &Path, dest: &Path) -> anyhow::Result<()> {
    // Retain the outgoing binary before it is replaced. A hard link keeps the
    // old inode alive at a second name without a copy, so this costs no disk
    // and cannot half-finish; a rename would leave the destination briefly
    // absent, which is worse than the problem it solves. A failure here is not
    // fatal — losing the rollback copy is better than failing the install — but
    // it is logged, because a silent failure would present as a rollback that
    // is simply missing when it is needed most.
    if dest.exists() {
        let prev = prev_sibling(dest);
        let _ = std::fs::remove_file(&prev);
        if let Err(e) = std::fs::hard_link(dest, &prev) {
            if let Err(e2) = std::fs::copy(dest, &prev) {
                tracing::warn!(
                    link_error = %e,
                    copy_error = %e2,
                    dest = %dest.display(),
                    "could not retain the previous binary; rollback will not cover it"
                );
            }
        }
    }
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

        // A pinned install resolves every asset from one release: `rev-<sha>`
        // for `--ref`, `v<X.Y.Z>` for stable. Probe that the release carries
        // binaries BEFORE the loop: without this the first Hard-gate binary
        // aborts with a bare "could not be installed", which reads as a broken
        // download rather than as the far likelier "that release never
        // published binaries". A `--ref` pin is read from `ctx.rev`, which
        // `venv_agent` has already expanded to a full object name.
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
        if let Some(pin) = pin.as_deref() {
            let sample = bins
                .iter()
                .find(|b| b.gate == Gate::Hard)
                .copied()
                .unwrap_or(binaries::default_vision_binary());
            sink.activity(self.id(), format!("checking the {pin} release"));
            if !pinned_release_published(pin, sample, &tmp_dir) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(match rev.as_deref() {
                    Some(rev) => rev_release_missing(rev),
                    None => version_release_missing(pin),
                });
            }
        }

        // `--artifacts <dir>`: validate the directory BEFORE the loop, for the
        // same reason the `--ref` probe above runs before it — a request that
        // cannot mean what the operator meant should cost nothing and say why.
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

        // Resolve each entry's byte source once, and SAY which ones came from
        // the local directory and which files were passed over. A fallback to the
        // release is legitimate (a one-crate rebuild) but it must never be
        // silent: these lines are what tell the operator that the binary they are
        // about to measure is theirs, and what makes a mistyped filename visible.
        let host_arch = ctx.env.arch.clone();
        let sources: Vec<AssetSource<'_>> = bins
            .iter()
            .map(|b| source_for(b, artifacts.as_deref(), &host_arch, pin.as_deref()))
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
            let ok = match install_service(b, &board_model, &tmp_dir, channel, &sink, source) {
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
    use crate::checkpoint::Checkpoint;

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
    fn no_channel_skips_the_signature_check_outright() {
        // `allow_unsigned` short-circuits inside `verify_artifact` BEFORE the
        // pubkey is read, so it is not "tolerate a missing signature" — it is
        // "do not look at signatures at all". Passing it on the default channel
        // meant the vendored trust anchor was never consulted and a binary
        // carrying a signature that does not match it was installed anyway.
        //
        // Tolerating a signature we cannot obtain is a separate, weaker
        // decision, and it already has its own home: `verify_minisign` routes a
        // missing `.minisig` (or a missing minisign binary) through
        // `unverifiable`, which warns on edge and refuses on stable. That is
        // what keeps an unsigned release installable today. Skipping the check
        // outright is never the right answer on any channel.
        assert!(
            !allow_unsigned_for(Channel::Edge),
            "the default channel must still consult the signing key"
        );
        assert!(!allow_unsigned_for(Channel::Stable));
    }

    #[test]
    fn an_unrecognised_channel_is_strict_not_lenient() {
        // The lenient branch must be opt-in by name, never a fallthrough. The
        // inverted test ("lenient unless exactly stable") reads the same for the
        // two channels we ship and silently hands the lenient branch to every
        // third value — a typo at the prompt, or a channel a newer build knows
        // and this one does not. A channel string we do not understand is not a
        // licence to skip a signature.
        for name in ["stabel", "beta", "", "STABLE", "Edge"] {
            let mut ctx = Ctx::for_test(Checkpoint::new());
            ctx.channel = name.to_string();
            assert_eq!(
                channel_of(&ctx),
                Channel::Stable,
                "unrecognised channel {name:?} must not get the lenient branch"
            );
        }
        // The one channel that IS lenient, by exact name.
        let mut ctx = Ctx::for_test(Checkpoint::new());
        ctx.channel = "edge".to_string();
        assert_eq!(channel_of(&ctx), Channel::Edge);
    }

    #[test]
    fn the_shell_and_rust_agree_on_which_channels_are_lenient() {
        // Two implementations of one policy: `ados_channel_is_lenient` in
        // `scripts/lib/verify.sh` gates the bootstrap and kernel-module fetches,
        // `channel_of` gates the prebuilt-binary fetch. They must not drift —
        // the shell side was already fixed to name its lenient channel
        // explicitly, and a divergence means one entry point verifies while the
        // other does not, which is worse than either posture chosen on purpose.
        let sh = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/lib/verify.sh")
            .canonicalize()
            .expect("scripts/lib/verify.sh must exist");
        let body = std::fs::read_to_string(&sh).unwrap();
        let lenient_shell = shell_lenient_channels(&body);
        assert!(
            !lenient_shell.is_empty(),
            "could not read the shell lenient set from {}",
            sh.display()
        );

        for name in ["edge", "stable", "beta", "stabel", ""] {
            let mut ctx = Ctx::for_test(Checkpoint::new());
            ctx.channel = name.to_string();
            let rust_lenient = channel_of(&ctx) == Channel::Edge;
            let shell_lenient = lenient_shell.iter().any(|c| c == name);
            assert_eq!(
                rust_lenient, shell_lenient,
                "channel {name:?}: shell lenient={shell_lenient}, rust lenient={rust_lenient}"
            );
        }
    }

    /// Extract the channel names `ados_channel_is_lenient` compares against, by
    /// reading the literals in its body. Shell parameter expansions (`${1:-}`)
    /// are not literals and are skipped.
    fn shell_lenient_channels(script: &str) -> Vec<String> {
        let after = match script.split_once("ados_channel_is_lenient() {") {
            Some((_, rest)) => rest,
            None => return Vec::new(),
        };
        let body = after.split_once("\n}").map(|(b, _)| b).unwrap_or(after);
        body.split('"')
            .skip(1)
            .step_by(2)
            .filter(|s| !s.contains('$'))
            .map(str::to_string)
            .collect()
    }

    /// Write an executable shell script at `path` with `body` as its content.
    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        set_executable(path).unwrap();
    }

    #[test]
    fn exec_probe_detects_a_rejection_that_fills_the_stderr_pipe() {
        // A loader complaint big enough to fill the ~64 KiB pipe buffer. Reading
        // stderr only after the process is reaped deadlocks here: the child
        // blocks writing, so it never exits, so nothing drains the pipe, so the
        // probe times out and calls a rejected binary runnable. Draining on a
        // separate thread is what makes this terminate at all.
        let dir = tempdir().unwrap();
        let script = dir.join("fake-chatty-vision");
        write_script(
            &script,
            "printf '%s\\n' \"fake-chatty-vision: /lib/libc.so.6: version GLIBC_2.34 not found\" >&2\n\
             i=0\n\
             while [ $i -lt 2000 ]; do printf '%s\\n' \"padding line to fill the pipe buffer\" >&2; i=$((i+1)); done\n\
             exit 1",
        );
        assert!(
            !binary_execs_on_this_host(&script),
            "a rejection must be detected even when the message fills the pipe"
        );
    }

    #[test]
    fn exec_probe_detects_a_glibc_rejection() {
        let dir = tempdir().unwrap();
        let script = dir.join("fake-onnx-vision");
        write_script(
            &script,
            "printf '%s\\n' \"fake-onnx-vision: /lib/aarch64-linux-gnu/libc.so.6: version GLIBC_2.34 not found (required by fake-onnx-vision)\" >&2\nexit 1",
        );
        assert!(
            !binary_execs_on_this_host(&script),
            "a GLIBC_-tagged nonzero exit must read as a linker rejection"
        );
    }

    #[test]
    fn exec_probe_accepts_a_binary_that_actually_starts() {
        let dir = tempdir().unwrap();
        let script = dir.join("fake-working-vision");
        // Sleeps well past the probe's deadline, mirroring a real service that
        // stays up — the probe must kill it and report success, not hang.
        // `exec` replaces the shell with `sleep` so the probe's kill() reaps
        // the actual long-lived process instead of orphaning it.
        write_script(&script, "exec sleep 5");
        assert!(
            binary_execs_on_this_host(&script),
            "a binary still running past the deadline was accepted by the loader"
        );
    }

    #[test]
    fn exec_probe_treats_an_unrelated_nonzero_exit_as_runnable() {
        let dir = tempdir().unwrap();
        let script = dir.join("fake-crashing-vision");
        // Fails fast, but for a reason that has nothing to do with the dynamic
        // loader (e.g. a real startup error against a missing config) — the
        // probe's job is narrowly the GLIBC_ signature, not "did it exit 0".
        write_script(&script, "echo 'config not found' >&2\nexit 1");
        assert!(
            binary_execs_on_this_host(&script),
            "a non-GLIBC_ failure must not be misread as a linker rejection"
        );
    }

    #[test]
    fn asset_base_hangs_an_unpinned_asset_off_its_rolling_tag() {
        // The exact URL, not its shape: this string is what a board resolves,
        // and every previous release-path defect was a wrong URL that still
        // looked plausible.
        assert_eq!(
            asset_base(None, "prebuilt-supervisor"),
            "https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-supervisor"
        );
    }

    #[test]
    fn asset_base_replaces_the_rolling_tag_with_the_per_revision_release() {
        let sha = "3b4b8deec0ffee1234567890abcdef1234567890";
        let pin = pinned_release("edge", Some(sha), None).unwrap();
        assert_eq!(pin.as_deref(), Some(format!("rev-{sha}").as_str()));
        assert_eq!(
            asset_base(pin.as_deref(), "prebuilt-supervisor"),
            format!("https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{sha}")
        );
        // The pin is what selects the release, so two services that differ only
        // by rolling tag resolve to the SAME per-revision base. That is the
        // property the flag exists for: one revision, one release, no chance of
        // a wheel from one commit beside a binary from another.
        assert_eq!(
            asset_base(pin.as_deref(), "prebuilt-supervisor"),
            asset_base(pin.as_deref(), "prebuilt-video")
        );
    }

    #[test]
    fn a_pinned_asset_url_is_the_rev_release_plus_the_unchanged_asset_name() {
        // The asset filename is NOT rewritten by a pin — CI re-uploads the
        // byte-identical set under the rev tag, so only the tag moves.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let pin = pinned_release("edge", Some(sha), None).unwrap();
        let b = PREBUILT
            .iter()
            .find(|b| b.service == "ados-supervisor")
            .unwrap();
        assert_eq!(
            format!("{}/{}", asset_base(pin.as_deref(), b.release_tag), b.asset),
            format!(
                "https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{sha}/ados-supervisor-aarch64"
            )
        );
    }

    #[test]
    fn stable_resolves_every_binary_from_its_own_version_release() {
        // The rolling tags are whatever main last built; a stable install must
        // place the binaries published with the release it installs, and a v-
        // prefixed or bare --version addresses the same release.
        for version in ["0.99.376", "v0.99.376"] {
            let pin = pinned_release("stable", None, Some(version)).unwrap();
            assert_eq!(pin.as_deref(), Some("v0.99.376"));
            let b = PREBUILT.iter().find(|b| b.service == "ados-video").unwrap();
            assert_eq!(
                format!("{}/{}", asset_base(pin.as_deref(), b.release_tag), b.asset),
                "https://github.com/altnautica/ADOSDroneAgent/releases/download/v0.99.376/ados-video-aarch64"
            );
        }
        // No version means nothing pinned to install: refused, never the
        // rolling tags.
        assert!(pinned_release("stable", None, None).is_err());
        assert!(pinned_release("stable", None, Some("  ")).is_err());
        // The development channel keeps the rolling tags unless --ref pins it.
        assert_eq!(pinned_release("edge", None, Some("0.99.376")), Ok(None));
    }

    #[test]
    fn the_rev_tag_is_the_one_ci_publishes() {
        assert_eq!(rev_release_tag("abc123"), "rev-abc123");
    }

    #[test]
    fn a_missing_rev_release_names_the_path_filter_as_the_reason() {
        // A bare 404 sends the operator looking for a network fault. The likely
        // cause is that the pinned commit touched only the Python tree, which
        // the Rust workflow's path filter does not build — unguessable unless
        // the message says it.
        let msg = rev_release_missing("3b4b8dee");
        assert!(msg.contains("rev-3b4b8dee"), "names the missing tag: {msg}");
        assert!(msg.contains("crates/**"), "names the filter: {msg}");
        assert!(
            msg.contains("src/ados/**"),
            "names the excluded tree: {msg}"
        );
        assert!(
            msg.contains("without --ref"),
            "names the way forward: {msg}"
        );
    }

    #[test]
    fn the_release_base_override_wins_only_when_it_says_something() {
        assert_eq!(
            base_or_default(Some("file:///tmp/fake-release")),
            "file:///tmp/fake-release"
        );
        assert_eq!(base_or_default(None), RELEASE_BASE);
        // An exported-but-empty variable is a common accident in a CI shell; it
        // must not turn every asset URL into a relative path.
        assert_eq!(base_or_default(Some("")), RELEASE_BASE);
        assert_eq!(base_or_default(Some("   ")), RELEASE_BASE);
    }

    #[test]
    fn the_release_base_override_is_the_variable_the_bootstrap_exports() {
        // Producer/reader symmetry: `scripts/install.sh` resolves the installer
        // binary from this same base and exports it so the install it execs
        // inherits it. A rename on either side would leave the shell half of a
        // file:// test pointed at the fake tree and the Rust half at GitHub —
        // which reads as a passing test and a fetch that never used the override.
        let bootstrap = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/install.sh")
            .canonicalize()
            .expect("the bootstrap must exist beside the crate it fetches");
        let text = std::fs::read_to_string(&bootstrap).expect("the bootstrap must be readable");
        assert!(
            text.contains(&format!("export {RELEASE_BASE_ENV}")),
            "{} must export {RELEASE_BASE_ENV} so the installer inherits the same base",
            bootstrap.display()
        );
        assert!(
            text.contains(&format!("{RELEASE_BASE_ENV}:-{RELEASE_BASE}")),
            "{} must default {RELEASE_BASE_ENV} to the release base this crate compiles in",
            bootstrap.display()
        );
    }

    #[test]
    fn only_a_full_object_name_can_address_a_per_revision_release() {
        let full = "3b4b8deec0ffee1234567890abcdef1234567890";
        assert!(is_full_object_name(full));
        assert!(!is_full_object_name(&full[..8]));
        assert!(!is_full_object_name("main"));
        assert!(!is_full_object_name(""));
        // And an abbreviated pin that slipped through says which step did not
        // run, instead of blaming the workflow's path filter for a skipped
        // checkout.
        let msg = rev_not_expanded("3b4b8dee");
        assert!(msg.contains("--force"), "names the way forward: {msg}");
        assert!(
            !msg.contains("crates/**"),
            "must not misattribute a skipped checkout to the path filter: {msg}"
        );
    }

    // ----- `--artifacts <dir>`: locally-built service binaries -----

    /// A minimal well-formed 64-bit little-endian ELF header declaring
    /// `machine`, followed by `payload` so two artifacts can differ in content.
    fn fake_elf(machine: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(20 + payload.len());
        v.extend_from_slice(b"\x7fELF");
        v.push(2); // EI_CLASS: 64-bit
        v.push(1); // EI_DATA: little-endian
        v.push(1); // EI_VERSION
        v.extend_from_slice(&[0u8; 9]); // EI_OSABI .. EI_PAD
        v.extend_from_slice(&2u16.to_le_bytes()); // e_type: ET_EXEC
        v.extend_from_slice(&machine.to_le_bytes()); // e_machine
        v.extend_from_slice(payload);
        v
    }

    fn sha256_sidecar_body(bytes: &[u8], name: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}  {name}\n", h.finalize())
    }

    /// Drop `bytes` into `dir` as `name`, with the `sha256sum`-format sidecar a
    /// build host would have produced beside it.
    fn write_local_artifact(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
        std::fs::write(
            dir.join(format!("{name}.sha256")),
            sha256_sidecar_body(bytes, name),
        )
        .unwrap();
    }

    /// The real `ados-video` catalog entry. Placement is driven through
    /// `install_one_at` with a tempdir dest, so the assertions exercise the
    /// shipped entry (asset name, gate, tag) rather than a fabricated one.
    fn video_entry() -> &'static PrebuiltBinary {
        PREBUILT.iter().find(|b| b.service == "ados-video").unwrap()
    }

    #[test]
    fn a_local_artifact_is_verified_placed_and_leaves_a_rollback_copy() {
        // The whole point of the flag: a bench node validating unlanded agent
        // code goes through the product's own install path, not a `cp` over
        // /opt/ados/bin. So this drives the REAL `install_one` — the same
        // function the fetched-asset path calls — and asserts the same four
        // outcomes it guarantees there: digest checked, binary executable,
        // destination atomically replaced, previous binary retained for
        // rollback.
        let host_arch = crate::env::arch();
        let Some(machine) = expected_elf_machine(host_arch) else {
            // An architecture the ELF gate has no opinion about; nothing to
            // assert about a host this test cannot build a header for.
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let dest = dst.path().join("ados-video");

        // A working binary is already installed; it is what rollback must keep.
        std::fs::write(&dest, fake_elf(machine, b"the binary already installed")).unwrap();

        let new_bytes = fake_elf(machine, b"the locally built binary");
        write_local_artifact(src.path(), "ados-video", &new_bytes);

        let source = AssetSource::Local {
            dir: src.path(),
            host_arch,
        };
        install_one_at(
            video_entry(),
            &dest,
            scratch.path(),
            Channel::Edge,
            &ProgressSink::default(),
            &source,
        )
        .expect("a local artifact with a matching .sha256 must install");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            new_bytes,
            "dest was replaced"
        );
        assert_eq!(
            std::fs::read(prev_sibling(&dest)).unwrap(),
            fake_elf(machine, b"the binary already installed"),
            "the outgoing binary must be retained at <dest>.prev for rollback"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "placed binary must be executable");
        }
        // No staging debris left behind.
        assert!(!dl_sibling(&dest).exists());
        assert!(!sidecar_path(&dl_sibling(&dest), "sha256").exists());
    }

    #[test]
    fn a_local_artifact_that_fails_its_digest_leaves_the_running_binary_alone() {
        // The verify gate is not decorative on this path. A file that does not
        // match the digest its build host recorded (a truncated scp, the wrong
        // binary under the right name) must be refused BEFORE the swap, so the
        // node keeps running the binary it already had — the same
        // verify-then-replace ordering the fetched path relies on.
        let host_arch = crate::env::arch();
        let Some(machine) = expected_elf_machine(host_arch) else {
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let dest = dst.path().join("ados-video");
        let installed = fake_elf(machine, b"the binary already installed");
        std::fs::write(&dest, &installed).unwrap();

        // Sidecar describes one file; the file on disk is a different one.
        write_local_artifact(
            src.path(),
            "ados-video",
            &fake_elf(machine, b"honest bytes"),
        );
        std::fs::write(
            src.path().join("ados-video"),
            fake_elf(machine, b"tampered bytes"),
        )
        .unwrap();

        let err = install_one_at(
            video_entry(),
            &dest,
            scratch.path(),
            Channel::Edge,
            &ProgressSink::default(),
            &AssetSource::Local {
                dir: src.path(),
                host_arch,
            },
        )
        .expect_err("a digest mismatch must refuse the install");
        assert!(
            err.to_string().contains("SHA256 verification failed"),
            "must name the check that refused it: {err}"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            installed,
            "a refused artifact must never reach the live path"
        );
        assert!(
            !dl_sibling(&dest).exists(),
            "the rejected staging copy must be cleared"
        );
    }

    #[test]
    fn a_local_artifact_without_a_sha256_is_refused_with_the_command_to_make_one() {
        // The installer will NOT hash the file and call that verified: a digest
        // computed from the bytes being checked proves nothing. So a missing
        // sidecar is a hard refusal, and the message has to carry the fix or the
        // operator's next move is to go looking for a way to skip the check.
        let host_arch = crate::env::arch();
        let Some(machine) = expected_elf_machine(host_arch) else {
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let dest = dst.path().join("ados-video");
        std::fs::write(
            src.path().join("ados-video"),
            fake_elf(machine, b"unsigned"),
        )
        .unwrap();

        let err = install_one_at(
            video_entry(),
            &dest,
            scratch.path(),
            Channel::Edge,
            &ProgressSink::default(),
            &AssetSource::Local {
                dir: src.path(),
                host_arch,
            },
        )
        .expect_err("no .sha256 must refuse the install");
        let msg = err.to_string();
        assert!(msg.contains("no .sha256"), "names what is missing: {msg}");
        assert!(msg.contains("sha256sum"), "names how to produce it: {msg}");
        assert!(!dest.exists(), "nothing may be placed on a refusal");
    }

    #[test]
    fn an_unsigned_local_artifact_installs_on_edge_and_is_refused_on_stable() {
        // This is the signature story stated exactly. A locally-built binary
        // cannot carry the CI signature (the key is a CI secret), so it reaches
        // `verify_minisign`'s "unverifiable" branch: a warning on edge, a refusal
        // on stable. Nothing special-cases the local path — it gets the same
        // policy a release asset published before signing existed gets, which is
        // why `--artifacts` is refused on stable at the command line instead of
        // being allowed to fail here, one binary at a time.
        let host_arch = crate::env::arch();
        let Some(machine) = expected_elf_machine(host_arch) else {
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        write_local_artifact(
            src.path(),
            "ados-video",
            &fake_elf(machine, b"unsigned build"),
        );
        let source = AssetSource::Local {
            dir: src.path(),
            host_arch,
        };

        let edge_dest = dst.path().join("edge-ados-video");
        install_one_at(
            video_entry(),
            &edge_dest,
            scratch.path(),
            Channel::Edge,
            &ProgressSink::default(),
            &source,
        )
        .expect("edge tolerates a signature it cannot obtain");
        assert!(edge_dest.exists());

        let stable_dest = dst.path().join("stable-ados-video");
        let err = install_one_at(
            video_entry(),
            &stable_dest,
            scratch.path(),
            Channel::Stable,
            &ProgressSink::default(),
            &source,
        )
        .expect_err("stable refuses an artifact it cannot signature-verify");
        assert!(
            err.to_string().contains("stable channel"),
            "must name the channel that refused it: {err}"
        );
        assert!(!stable_dest.exists());
    }

    #[test]
    fn a_binary_built_for_the_wrong_machine_is_refused_before_it_replaces_anything() {
        // The bench mistake this catches: `cargo build --release` on the
        // developer's Mac, then scp the Mach-O (or an x86_64 ELF) to the node.
        // Without the gate that lands at /opt/ados/bin and surfaces as a
        // crash-looping unit with an exec-format error.
        assert_eq!(artifact_arch_error(&fake_elf(0xB7, b"x"), "aarch64"), None);
        let wrong = artifact_arch_error(&fake_elf(0x3E, b"x"), "aarch64")
            .expect("an x86_64 ELF cannot run on an aarch64 node");
        assert!(wrong.contains("aarch64"), "{wrong}");

        // A Mach-O (the `\xcf\xfa\xed\xfe` magic a Mac build carries).
        let macho = artifact_arch_error(b"\xcf\xfa\xed\xfe0123456789abcdef", "aarch64")
            .expect("a Mach-O is not an ELF");
        assert!(macho.contains("not an ELF"), "{macho}");

        // A host architecture the gate has no table entry for is not grounds for
        // refusing an artifact; the check simply does not run.
        assert_eq!(artifact_arch_error(&fake_elf(0x3E, b"x"), "s390x"), None);
    }

    #[test]
    fn a_directory_that_supplies_nothing_fails_and_build_debris_is_only_reported() {
        // The single-file typo is fatal, because the silent outcome is the worst
        // one available here: that service falls back to the release, the install
        // reports success, and the bench measures the OLD binary believing it
        // measured the new one.
        let typo = tempfile::tempdir().unwrap();
        std::fs::write(typo.path().join("ados-vidoe"), b"typo").unwrap();
        let err = validate_artifacts_dir(typo.path())
            .expect_err("a directory supplying no service binary must fail the step");
        assert!(
            err.contains("holds no service binary"),
            "names the problem: {err}"
        );
        assert!(
            err.contains("ados-video-aarch64"),
            "names an accepted spelling: {err}"
        );

        // But a real `target/release` is the natural thing to point the flag at,
        // and it carries dep files, rlibs and the installer itself. Those are
        // reported and passed over, never grounds for refusing the directory —
        // rejecting them would make the flag unusable for its only purpose.
        let target = tempfile::tempdir().unwrap();
        for name in [
            "ados-video",
            "ados-video.d",
            "ados-installer",
            "libados_config.rlib",
        ] {
            std::fs::write(target.path().join(name), b"x").unwrap();
        }
        let passed_over = validate_artifacts_dir(target.path())
            .expect("a build directory carrying one service binary is usable");
        assert_eq!(
            passed_over,
            vec![
                "ados-installer".to_string(),
                "ados-video.d".to_string(),
                "libados_config.rlib".to_string()
            ],
            "everything that is not a service binary is named, so a mistyped \
             filename shows up in the report"
        );

        // Both accepted spellings are recognised, and their sidecars are not
        // mistaken for artifacts of their own.
        assert!(ignored_artifact_names(&[
            "ados-video".to_string(),
            "ados-supervisor-aarch64".to_string(),
            "ados-video.sha256".to_string(),
            "ados-video.minisig".to_string(),
        ])
        .is_empty());

        // An empty directory and a path that is not one are both wrong paths.
        let empty = tempfile::tempdir().unwrap();
        assert!(validate_artifacts_dir(empty.path()).is_err());
        assert!(validate_artifacts_dir(&typo.path().join("nope")).is_err());
    }

    #[test]
    fn a_service_the_directory_does_not_carry_still_comes_from_the_release() {
        // One rebuilt crate means one copied file, not fifteen. The entries the
        // directory does not carry resolve to the release exactly as they would
        // with no flag at all.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ados-video"), b"local").unwrap();
        let video = PREBUILT.iter().find(|b| b.service == "ados-video").unwrap();
        let supervisor = PREBUILT
            .iter()
            .find(|b| b.service == "ados-supervisor")
            .unwrap();

        assert!(matches!(
            source_for(video, Some(dir.path()), "aarch64", None),
            AssetSource::Local { .. }
        ));
        assert!(matches!(
            source_for(supervisor, Some(dir.path()), "aarch64", None),
            AssetSource::Release { pin: None }
        ));
        // With no flag every entry is a release fetch, pin and all.
        assert!(matches!(
            source_for(video, None, "aarch64", Some("rev-abc")),
            AssetSource::Release {
                pin: Some("rev-abc")
            }
        ));
        // The release asset name is accepted as well as the service name.
        std::fs::write(dir.path().join("ados-supervisor-aarch64"), b"local").unwrap();
        assert!(matches!(
            source_for(supervisor, Some(dir.path()), "aarch64", None),
            AssetSource::Local { .. }
        ));
    }

    #[test]
    fn the_step_refuses_a_bad_artifacts_directory_before_it_installs_anything() {
        // Wiring, not policy: the validation is reached from `Ctx::artifacts` and
        // runs BEFORE the install loop, so a wrong path costs nothing and places
        // nothing. Without this the flag could be parsed, carried onto the
        // context and never read, and every test above would still pass.
        let mut ctx = Ctx::for_test(Checkpoint::new());
        if !ctx.env.supported_arch {
            // The step skips a non-aarch64 host before it looks at anything.
            return;
        }
        ctx.artifacts = Some(PathBuf::from(
            "/nonexistent/ados-artifacts-that-cannot-be-a-directory",
        ));
        match FetchBinaries.run(&mut ctx) {
            StepOutcome::Failed(msg) => {
                assert!(msg.contains("--artifacts"), "names the flag: {msg}");
                assert!(msg.contains("not a directory"), "names the fault: {msg}");
            }
            other => panic!("expected the step to fail before installing: {other:?}"),
        }
    }

    #[test]
    fn a_local_artifact_failure_is_reported_once_rather_than_retried() {
        // The retry loop exists for a dropping link. A missing file or a bad
        // digest is terminal, so retrying it only puts the backoff between the
        // operator and the message naming what to fix.
        assert_eq!(AssetSource::Release { pin: None }.max_attempts(), 3);
        assert_eq!(
            AssetSource::Local {
                dir: Path::new("/tmp"),
                host_arch: "aarch64"
            }
            .max_attempts(),
            1
        );
    }
}
