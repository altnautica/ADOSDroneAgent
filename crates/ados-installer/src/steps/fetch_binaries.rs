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

/// Environment override for [`RELEASE_BASE`], for tests only.
///
/// The whole fetch stack is anonymous `curl`, which speaks `file://`, so a fake release
/// tree on disk gives the pinned + unpinned resolution a full end-to-end exercise with
/// no network and no credential. It follows the driver layer's `ADOS_PREBUILT_BASE_URL`
/// (`scripts/drivers/lib-prebuilt.sh`), including the part that matters: an operator
/// never sets it, and nothing in the install writes it.
pub const RELEASE_BASE_ENV: &str = "ADOS_RELEASE_BASE";

/// The release-download base in force for this run.
fn release_base() -> String {
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

/// Where one asset's URL hangs off: the rolling per-service tag normally, or the
/// commit's per-revision release when the install is pinned with `--ref`.
///
/// This is the SOLE place a prebuilt asset URL gets its base, so a pin cannot
/// half-apply: every binary in the catalog, the onnx vision variant, and the
/// ONNX Runtime library all resolve through here. `rev` is already the full
/// object name — `venv_agent` expands an abbreviated `--ref` from the clone
/// before this step runs, so no prefix is ever handed to a server that cannot
/// resolve one.
pub fn asset_base(rev: Option<&str>, tag: &str) -> String {
    match rev {
        Some(rev) => format!("{}/{}", release_base(), rev_release_tag(rev)),
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

/// Whether the per-revision release for `rev` actually carries assets.
///
/// One sidecar fetch answers it: `<base>/rev-<sha>/<asset>.sha256` exists only
/// if CI published that revision. `sample` is a Hard-gated catalog entry, so a
/// present sidecar means the release holds the assets the install cannot do
/// without — not merely that a tag of that name exists. Cheap enough to pay for
/// on every pinned install (a few hundred bytes) and it converts an opaque
/// per-binary 404 into the one message that names the cause.
fn rev_release_published(rev: &str, sample: &PrebuiltBinary, tmp_dir: &Path) -> bool {
    let url = format!(
        "{}/{}.sha256",
        asset_base(Some(rev), sample.release_tag),
        sample.asset
    );
    let probe = tmp_dir.join("rev-release-probe.sha256");
    let ok = net::fetch(&url, &probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

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

/// Fetch + verify one prebuilt binary, then place it atomically at its
/// destination. Returns `Ok(())` on success, `Err` on any fetch/verify/place
/// miss (the caller maps that through the gate). `tmp_dir` holds nothing for the
/// binary itself — the binary is fetched to a `.dl` sibling of the real dest so
/// the final placement is a same-filesystem `rename` (see [`place_binary`]); the
/// dir is retained for callers that want a scratch root and for symmetry.
/// `rev` is the `--ref` pin, which moves every URL off the rolling tag and onto
/// that commit's per-revision release (see [`asset_base`]).
fn install_one(
    b: &PrebuiltBinary,
    _tmp_dir: &Path,
    channel: Channel,
    sink: &ProgressSink,
    rev: Option<&str>,
) -> anyhow::Result<()> {
    let asset_url = format!("{}/{}", asset_base(rev, b.release_tag), b.asset);
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

        // Verify the downloaded temp BEFORE it is placed at the live path. Every
        // channel checks any `.minisig` that arrived against the vendored trust
        // anchor; only whether a MISSING one is fatal still varies by channel.
        //
        // The best-effort `.minisig` fetch above is what makes that safe to run
        // on the default channel today: curl is invoked with `-f`, so a 404
        // leaves no file at all rather than a saved error page, and `net::fetch`
        // never promotes a failed transfer to the destination. An absent sidecar
        // therefore reads as "unobtainable" (warn on edge) and not as a
        // signature that fails to verify.
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
    rev: Option<&str>,
) -> anyhow::Result<()> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut backoff = std::time::Duration::from_secs(1);
    for attempt in 1..=MAX_ATTEMPTS {
        match install_one(b, tmp_dir, channel, sink, rev) {
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
    rev: Option<&str>,
) -> anyhow::Result<()> {
    if b.service == "ados-vision" && binaries::board_prefers_onnx_vision(board_model) {
        // The onnx binary links the ONNX Runtime dynamically, so the binary AND
        // its shared library are installed together — either both land or the
        // install falls back to the default (musl, no-onnx) build. Installing the
        // onnx binary without its runtime would leave a vision service that
        // cannot dlopen ORT at start.
        match install_onnx_vision(tmp_dir, channel, sink, rev) {
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
    install_one_with_retry(b, tmp_dir, channel, sink, rev)
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
    rev: Option<&str>,
) -> anyhow::Result<()> {
    install_one_with_retry(&binaries::PREBUILT_VISION_ONNX, tmp_dir, channel, sink, rev)?;
    install_one_with_retry(
        &binaries::PREBUILT_VISION_ONNX_RUNTIME,
        tmp_dir,
        channel,
        sink,
        rev,
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

        // A pinned install resolves every asset from `rev-<sha>`. Probe that the
        // release exists BEFORE the loop: without this the first Hard-gate
        // binary aborts with a bare "could not be installed", which reads as a
        // broken download rather than as the far likelier "that commit never
        // published binaries" (see `rev_release_missing`). The pin is read from
        // `ctx.rev`, which `venv_agent` has already expanded to a full object
        // name.
        let rev = ctx.rev.clone();
        if let Some(rev) = rev.as_deref() {
            if !is_full_object_name(rev) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(rev_not_expanded(rev));
            }
            let sample = bins
                .iter()
                .find(|b| b.gate == Gate::Hard)
                .copied()
                .unwrap_or(binaries::default_vision_binary());
            sink.activity(
                self.id(),
                format!("checking the {} release", rev_release_tag(rev)),
            );
            if !rev_release_published(rev, sample, &tmp_dir) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(rev_release_missing(rev));
            }
        }

        let total = bins.len() as u64;
        sink.sub_progress(self.id(), 0, total);
        for (i, b) in bins.into_iter().enumerate() {
            sink.activity(self.id(), format!("installing {}", b.service));
            let ok = match install_service(
                b,
                &board_model,
                &tmp_dir,
                channel,
                &sink,
                rev.as_deref(),
            ) {
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
        assert_eq!(
            asset_base(Some(sha), "prebuilt-supervisor"),
            format!("https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{sha}")
        );
        // The pin is what selects the release, so two services that differ only
        // by rolling tag resolve to the SAME per-revision base. That is the
        // property the flag exists for: one revision, one release, no chance of
        // a wheel from one commit beside a binary from another.
        assert_eq!(
            asset_base(Some(sha), "prebuilt-supervisor"),
            asset_base(Some(sha), "prebuilt-video")
        );
    }

    #[test]
    fn a_pinned_asset_url_is_the_rev_release_plus_the_unchanged_asset_name() {
        // The asset filename is NOT rewritten by a pin — CI re-uploads the
        // byte-identical set under the rev tag, so only the tag moves.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let b = PREBUILT
            .iter()
            .find(|b| b.service == "ados-supervisor")
            .unwrap();
        assert_eq!(
            format!("{}/{}", asset_base(Some(sha), b.release_tag), b.asset),
            format!(
                "https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{sha}/ados-supervisor-aarch64"
            )
        );
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
}
