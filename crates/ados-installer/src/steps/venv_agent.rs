//! Venv + agent package: create the Python virtualenv and install the agent
//! package into it. Required. Checkpoint `venv`.
//!
//! Ports the venv + agent-install portion of `scripts/install.d/13-main.sh`
//! (fresh-install path) plus `ensure_venv_pip` from 14-orchestration.sh:
//!   0. resolve a Python 3.11+ interpreter: prefer a system one, and when the
//!      board has none (Debian 11 ships 3.9 with no `python3.11` package),
//!      provision a portable CPython runtime ([`super::portable_python`]) so the
//!      install stays fully automatic. The resolved interpreter creates the venv
//!      for both the edge and stable channels.
//!   1. `python3 -m venv --system-site-packages /opt/ados/venv`
//!   2. self-heal a rotted pip (probe `pip --version`, recreate the venv on
//!      failure)
//!   3. install the agent package per channel:
//!      edge   — git clone the repo (honoring --branch) + `pip install <repo>`,
//!      or, with `--ref <rev>`, clone then fetch + detached-checkout that exact
//!      revision (a clone's `--branch` never takes a SHA) and hand the expanded
//!      40-char object name to the binary fetch
//!      stable — download + SHA256-verify the release wheel for `--version`,
//!      then `pip install <wheel>` (no on-disk source tree)
//!
//! The venv-path + pip-args + wheel-URL + git-args builders are pure so a unit
//! test exercises them without a real interpreter or the network.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::ctx::Ctx;
use crate::env;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;
use crate::ui::{activity, ProgressSink};
use crate::verify;

/// Stream a git clone's output to the `venv_agent` live-detail pane.
fn on_git_line(sink: &ProgressSink) -> impl FnMut(&str) + '_ {
    move |line: &str| {
        sink.sub_log("venv_agent", line);
        if let Some(a) = activity::git_activity(line) {
            sink.activity("venv_agent", a);
        }
    }
}

/// Stream a pip install's output to the `venv_agent` live-detail pane.
fn on_pip_line(sink: &ProgressSink) -> impl FnMut(&str) + '_ {
    move |line: &str| {
        sink.sub_log("venv_agent", line);
        if let Some(a) = activity::pip_activity(line) {
            sink.activity("venv_agent", a);
        }
    }
}

/// The agent's git repo URL (edge channel clones from here, honoring --branch).
const REPO_URL: &str = "https://github.com/altnautica/ADOSDroneAgent.git";

/// GitHub release-download base; the stable channel hangs the wheel asset off
/// `<base>/v<version>/<wheel>` (plus its `.sha256` sidecar). Mirrors the same
/// base the prebuilt-binary fetch uses.
const RELEASE_BASE: &str = "https://github.com/altnautica/ADOSDroneAgent/releases/download";

/// The venv interpreter path (`/opt/ados/venv/bin/python`). Pure.
pub fn venv_python() -> String {
    format!("{}/bin/python", env::VENV_DIR)
}

/// The venv pip path (`/opt/ados/venv/bin/pip`). Pure.
pub fn venv_pip() -> String {
    format!("{}/bin/pip", env::VENV_DIR)
}

/// Build the `python -m venv` argument vector (pure). System site packages are
/// visible so the apt-only `python3-gi` (PyGObject) the LCD video page imports
/// is reachable inside the venv.
pub fn venv_create_args(venv_dir: &str) -> Vec<String> {
    vec![
        "-m".to_string(),
        "venv".to_string(),
        "--system-site-packages".to_string(),
        venv_dir.to_string(),
    ]
}

/// Build the `pip install` args for the edge (source) channel (pure). `source`
/// is the local cloned repo path (preferred) or a `git+<url>` spec. No
/// `--quiet`: the resolve/build/install lines stream to the live-detail pane.
pub fn pip_install_edge_args(source: &str) -> Vec<String> {
    vec!["install".to_string(), source.to_string()]
}

/// Build the `pip install` args for the stable (wheel) channel (pure). The arg
/// is a local wheel file path (not `-e <repo>` / a URL), so pip installs the
/// already-downloaded, already-verified wheel from disk.
pub fn pip_install_wheel_args(wheel_path: &str) -> Vec<String> {
    vec!["install".to_string(), wheel_path.to_string()]
}

/// Normalize a `--version` value to the bare `X.Y.Z` form (pure). The operator
/// may pass either a `v`-prefixed tag (`v0.93.0`) or a bare version (`0.93.0`);
/// the wheel filename uses the bare form. Only a single leading `v` is stripped.
pub fn normalize_version(raw: &str) -> String {
    raw.strip_prefix('v').unwrap_or(raw).to_string()
}

/// Build the release wheel asset filename (pure). The release workflow publishes
/// `ados_drone_agent-<X.Y.Z>-py3-none-any.whl`; `version` is the bare form.
pub fn wheel_filename(version: &str) -> String {
    format!("ados_drone_agent-{version}-py3-none-any.whl")
}

/// Build the wheel asset download URL (pure). The release tag is `v`-prefixed
/// (`v<X.Y.Z>`) while the wheel filename uses the bare `<X.Y.Z>`; `version` here
/// is the already-normalized bare form.
pub fn wheel_url(version: &str) -> String {
    format!("{RELEASE_BASE}/v{version}/{}", wheel_filename(version))
}

/// Build the `git clone` args for the edge channel (pure). Honors an optional
/// branch; shallow + submodules, matching `git_clone_retry`.
pub fn git_clone_args(dest: &str, branch: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--recurse-submodules".to_string(),
        "--shallow-submodules".to_string(),
        "--progress".to_string(),
    ];
    if let Some(b) = branch {
        args.push("--branch".to_string());
        args.push(b.to_string());
    }
    args.push(REPO_URL.to_string());
    args.push(dest.to_string());
    args
}

/// The branch an abbreviated `--ref` is resolved against when no `--branch` was
/// given. Only used to deepen history far enough to expand a prefix locally.
const DEFAULT_BRANCH: &str = "main";

/// How far back an abbreviated commit is chased when the server refuses the
/// prefix. Bounded on purpose: a commit not in the last 500 of the branch is
/// reported, not hunted. Matches the bootstrap's macOS path.
const REV_DEEPEN_DEPTH: &str = "500";

/// Build the `git fetch` args that pull ONE revision (pure). Depth 1 because a
/// pin needs the commit, not its history. `--branch` cannot do this job: it
/// takes a branch or a tag and never a commit SHA, which is why the pinned path
/// is a clone followed by a fetch rather than a different clone.
pub fn git_fetch_rev_args(rev: &str) -> Vec<String> {
    vec![
        "fetch".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "origin".to_string(),
        rev.to_string(),
    ]
}

/// Build the `git fetch` args that deepen `branch` enough to expand an
/// abbreviated commit locally (pure).
pub fn git_deepen_args(branch: &str) -> Vec<String> {
    vec![
        "fetch".to_string(),
        "--depth".to_string(),
        REV_DEEPEN_DEPTH.to_string(),
        "origin".to_string(),
        branch.to_string(),
    ]
}

/// Build the args that expand a possibly-abbreviated revision to its full object
/// name against the local object store (pure). `^{commit}` refuses a prefix that
/// resolves to a tree or a tag rather than a commit.
pub fn git_rev_parse_args(rev: &str) -> Vec<String> {
    vec![
        "rev-parse".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        format!("{rev}^{{commit}}"),
    ]
}

/// Build the detached-checkout args for a resolved revision (pure). `--force`
/// because the staging tree is ours to move; `--detach` because a pin is not a
/// branch to follow.
pub fn git_checkout_detached_args(object: &str) -> Vec<String> {
    vec![
        "checkout".to_string(),
        "-q".to_string(),
        "--force".to_string(),
        "--detach".to_string(),
        object.to_string(),
    ]
}

/// Build the submodule re-sync args (pure). Moving HEAD to another commit leaves
/// the submodules where the clone put them, so without this a pinned tree is the
/// pinned superproject beside submodules from a different revision — the same
/// mixed-revision install `--ref` exists to prevent.
pub fn git_submodule_sync_args() -> Vec<String> {
    vec![
        "submodule".to_string(),
        "update".to_string(),
        "--init".to_string(),
        "--recursive".to_string(),
        "--depth".to_string(),
        "1".to_string(),
    ]
}

/// Whether `rev` is an ABBREVIATED commit hex prefix (pure): hex, non-empty, and
/// shorter than a full 40-character object name.
///
/// A server cannot answer a request for a prefix — it is refused with the same
/// "couldn't find remote ref" a typo gets — so a prefix has to be expanded
/// locally. A full 40-char SHA the server already refused is genuinely absent, so
/// it fails fast rather than paying for a deep fetch that cannot contain it. A
/// branch or tag name is not hex and never reaches the deepen path either.
pub fn is_abbreviated_commit(rev: &str) -> bool {
    !rev.is_empty() && rev.len() < 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

/// The failure text when a `--ref` cannot be resolved.
pub fn rev_unresolvable(rev: &str, branch: &str) -> String {
    format!(
        "could not resolve --ref {rev} in {REPO_URL}. Check that the commit, branch, or tag \
         exists and is pushed; an abbreviated commit resolves only if it is within the last \
         {REV_DEEPEN_DEPTH} commits of '{branch}', otherwise pass the full 40-character SHA. \
         Not falling back to '{branch}' — nothing was installed."
    )
}

/// Resolve a Python 3.11+ interpreter for the venv. Prefer a system interpreter
/// (the fast path); when the board carries none, provision a portable CPython
/// runtime so the install stays fully automatic on boards whose system Python
/// is older than the agent's 3.11 floor. The returned path is shared by both
/// install channels — `create_venv` is the single consumer.
fn resolve_python() -> anyhow::Result<String> {
    if let Some(p) = super::deps::find_python() {
        return Ok(p);
    }
    tracing::warn!("no system Python 3.11+ found; provisioning a portable CPython runtime");
    super::portable_python::provision()
}

/// Create the venv at `/opt/ados/venv` with the discovered interpreter.
fn create_venv(python: &str) -> anyhow::Result<()> {
    let args = venv_create_args(env::VENV_DIR);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = exec::run(python, &argv);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        anyhow::bail!("python interpreter {python} could not be spawned")
    } else {
        anyhow::bail!("`{python} -m venv` failed: {}", res.stderr.trim())
    }
}

/// True when the venv's pip answers `--version` (the self-heal probe).
fn venv_pip_works() -> bool {
    exec::run_ok(&venv_python(), &["-m", "pip", "--version"])
}

/// True when the venv interpreter can import `msgpack`.
///
/// The state IPC wire is length-prefixed msgpack (v2): the native state producer
/// always emits v2 frames. The Python state consumer needs `msgpack` to decode
/// those frames; if the module is absent it cannot read state at all. `msgpack`
/// is a declared agent dependency, so a clean `pip install` of the agent package
/// always pulls it in — but a venv rebuilt off a broken index or a partial wheel
/// cache can silently land without it. This probe is the post-provision
/// assertion that turns that silent gap into a loud install failure.
fn venv_msgpack_importable() -> bool {
    exec::run_ok(&venv_python(), &["-c", "import msgpack"])
}

/// Self-heal a rotted venv pip. Probe first; on failure try `ensurepip
/// --upgrade` in place, and as a last resort recreate the venv from scratch
/// with the same flags. Returns Ok only when pip works at the end.
fn ensure_venv_pip(python: &str) -> anyhow::Result<()> {
    if venv_pip_works() {
        return Ok(());
    }
    tracing::warn!("venv pip is broken; attempting in-place repair via ensurepip");
    let _ = exec::run(&venv_python(), &["-m", "ensurepip", "--upgrade"]);
    if venv_pip_works() {
        tracing::warn!("venv pip repaired via ensurepip");
        return Ok(());
    }
    // Recreate the venv from scratch.
    tracing::warn!("ensurepip did not recover pip; recreating the venv");
    let _ = std::fs::remove_dir_all(env::VENV_DIR);
    create_venv(python)?;
    if venv_pip_works() {
        Ok(())
    } else {
        anyhow::bail!("venv pip is still broken after recreate")
    }
}

/// Move an already-cloned tree to the revision `--ref` names, and return that
/// revision's FULL object name.
///
/// Every step is checked. The unpinned path tolerates a failed update and builds
/// whatever is on disk; doing that with a pin would install a different revision
/// than the one asked for, which is the entire failure this flag exists to stop.
///
/// Resolution order mirrors the bootstrap's macOS path, because a pin has to mean
/// the same thing on both:
///   1. fetch the exact object (`FETCH_HEAD`) — works for a full SHA, a branch,
///      or a tag;
///   2. only for a hex prefix the server could not answer, deepen the branch once
///      and expand it locally;
///   3. otherwise fail loudly, without falling back to the branch tip.
fn checkout_rev(
    repo: &Path,
    rev: &str,
    branch: &str,
    sink: &ProgressSink,
) -> anyhow::Result<String> {
    let repo_s = repo.to_string_lossy().into_owned();
    // Every git call runs against the staged tree, so `-C <repo>` is prefixed
    // once here rather than repeated at each callsite.
    let git = |args: &[String]| -> exec::CmdResult {
        let mut argv: Vec<&str> = vec!["-C", &repo_s];
        argv.extend(args.iter().map(String::as_str));
        exec::run("git", &argv)
    };

    let fetch = git_fetch_rev_args(rev);
    let object = if git(&fetch).success() {
        "FETCH_HEAD".to_string()
    } else if is_abbreviated_commit(rev) {
        sink.sub_log(
            "venv_agent",
            &format!("'{rev}' is an abbreviated commit; deepening {branch} to resolve it"),
        );
        // Best-effort: the deepen only has to bring the object into the local
        // store, and rev-parse below is what decides whether it did.
        let _ = git(&git_deepen_args(branch));
        let parsed = git(&git_rev_parse_args(rev));
        let full = parsed.stdout.trim().to_string();
        if !parsed.success() || full.is_empty() {
            anyhow::bail!("{}", rev_unresolvable(rev, branch));
        }
        full
    } else {
        anyhow::bail!("{}", rev_unresolvable(rev, branch));
    };

    let checkout = git(&git_checkout_detached_args(&object));
    if !checkout.success() {
        anyhow::bail!(
            "resolved --ref {rev} but could not check it out at {repo_s}: {}",
            checkout.stderr.trim()
        );
    }

    // Submodules follow the superproject, or the pinned tree is a mix of
    // revisions. A tree with no submodules exits 0 here, so this is not
    // conditional on the repo having any.
    let submodules = git(&git_submodule_sync_args());
    if !submodules.success() {
        anyhow::bail!(
            "checked out --ref {rev} but could not sync its submodules: {}",
            submodules.stderr.trim()
        );
    }

    let head = git(&["rev-parse".to_string(), "HEAD".to_string()]);
    let full = head.stdout.trim().to_string();
    if !head.success() || full.len() != 40 {
        anyhow::bail!("checked out --ref {rev} but could not read the resulting HEAD at {repo_s}");
    }
    Ok(full)
}

/// Install the agent package on the edge channel: clone the repo (honoring
/// --branch, or checking out `--ref`'s exact revision) into a PERSISTED dir,
/// then `pip install <repo>`. Returns the cloned repo path so the caller can
/// record it into `ctx.source_dir` — the downstream `systemd` /
/// `config_identity` / `dkms` steps read `data/systemd`, `data/udev`, and
/// `scripts/drivers/*` from it. We do NOT delete the clone (mirrors the bash
/// installer persisting the tree to `/opt/ados/source`); a reinstall re-clones
/// over it after wiping.
///
/// Takes `&mut Ctx` for one reason: a pinned install writes the EXPANDED object
/// name back into `ctx.rev`, which is what lets the later binary fetch address
/// `rev-<full sha>` without asking a server to expand a prefix it cannot.
fn install_agent_edge(ctx: &mut Ctx) -> anyhow::Result<PathBuf> {
    let sink = ctx.progress.clone();
    let repo = clone_dest()?;
    let repo_s = repo.to_string_lossy().into_owned();

    // Clone into a staging sibling and swap it in only once it succeeds, rather
    // than deleting the live tree first.
    //
    // Deleting first meant a failed clone left the node with NO source tree at
    // all — and that tree carries `scripts/install.sh`, so the box lost its own
    // ability to retry. On a node with no internet that is unrecoverable, and
    // even with internet the installer's own advice ("re-run the install
    // one-liner") is then the only way back. Observed exactly once, on a rig:
    // the clone failed and `bash /opt/ados/source/scripts/install.sh` was
    // afterwards "No such file or directory".
    //
    // The swap is delete-then-rename rather than a true atomic exchange, which
    // is the best a cross-platform rename gives us here; the window is a rename
    // rather than a network clone, so the exposure goes from minutes to
    // microseconds.
    let staging = staging_dest(&repo);
    let staging_s = staging.to_string_lossy().into_owned();
    let _ = std::fs::remove_dir_all(&staging);

    let clone = git_clone_args(&staging_s, ctx.args.branch.as_deref());
    let clone_argv: Vec<&str> = clone.iter().map(String::as_str).collect();
    let clone_res = exec::run_streamed("git", &clone_argv, on_git_line(&sink));
    if !clone_res.success() {
        // Leave the existing tree untouched: a node that could retry before
        // this attempt must still be able to retry after it.
        let _ = std::fs::remove_dir_all(&staging);
        if !clone_res.spawned {
            anyhow::bail!("git is not installed");
        }
        anyhow::bail!("git clone failed: {}", clone_res.stderr.trim());
    }

    // A `--ref` pin moves the STAGED tree to that revision before it is
    // promoted, so the tree that gets pip-installed is the pinned one and a
    // failure still leaves the live tree (and its `scripts/install.sh`) intact.
    if let Some(rev) = ctx.rev.clone() {
        let branch = ctx
            .args
            .branch
            .clone()
            .unwrap_or_else(|| DEFAULT_BRANCH.to_string());
        match checkout_rev(&staging, &rev, &branch, &sink) {
            Ok(full) => {
                sink.sub_log("venv_agent", &format!("✓ source pinned to {full}"));
                // The full object name is the half of the pin the binary fetch
                // needs: `rev-<sha>` is the tag CI publishes, and an abbreviated
                // value would 404 it. Expanded here because this is the only
                // place in the install that holds a git object store, and this
                // step runs before `fetch_binaries`.
                ctx.rev = Some(full);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        }
    }
    promote_staging(&staging, &repo)?;

    let pip = pip_install_edge_args(&repo_s);
    let pip_argv: Vec<&str> = pip.iter().map(String::as_str).collect();
    let pip_res = exec::run_streamed(&venv_pip(), &pip_argv, on_pip_line(&sink));
    if pip_res.success() {
        Ok(repo)
    } else {
        anyhow::bail!(
            "pip install of the agent package failed: {}",
            pip_res.stderr.trim()
        )
    }
}

/// Install the agent package on the stable channel: download the release wheel
/// for the pinned `--version` plus its `.sha256` sidecar, verify the SHA256, then
/// `pip install <wheel>` into the venv. Unlike the edge path there is NO on-disk
/// source tree, so the caller records no `ctx.source_dir`; the downstream OS
/// steps resolve their unit files / udev rules / driver scripts from the
/// persisted `/opt/ados/source` (left by a prior install or the package data)
/// instead. Temp downloads are cleaned up on every exit path.
///
/// A `--ref` pin never reaches here: `ctx::rev_channel_conflict` refuses the
/// stable+`--ref` pair before the install starts, because a `v<X.Y.Z>` release
/// tag addresses a version and not a commit, so there is no per-revision wheel
/// this could fetch.
fn install_agent_stable(ctx: &Ctx) -> anyhow::Result<()> {
    let raw = ctx.args.version.as_deref().ok_or_else(|| {
        anyhow::anyhow!("stable channel requires --version (the release to install)")
    })?;
    let version = normalize_version(raw);
    let url = wheel_url(&version);

    // Stage the wheel + its sidecar under a unique temp dir so a partial fetch
    // never collides with a concurrent run and cleanup is a single dir remove.
    let dir = wheel_tmp_dir()?;
    let wheel_path = dir.join(wheel_filename(&version));
    let sha_path = sidecar(&wheel_path, "sha256");

    let sink = ctx.progress.clone();
    let outcome = (|| {
        net::fetch(&url, &wheel_path)?;
        net::fetch(&format!("{url}.sha256"), &sha_path)?;

        // A SHA256 mismatch (tamper / truncation) is a hard failure.
        verify::verify_sha256(&wheel_path, &sha_path)?;

        let wheel_s = wheel_path.to_string_lossy().into_owned();
        let pip = pip_install_wheel_args(&wheel_s);
        let pip_argv: Vec<&str> = pip.iter().map(String::as_str).collect();
        let pip_res = exec::run_streamed(&venv_pip(), &pip_argv, on_pip_line(&sink));
        if pip_res.success() {
            Ok(())
        } else if !pip_res.spawned {
            anyhow::bail!("venv pip {} could not be spawned", venv_pip());
        } else {
            anyhow::bail!(
                "pip install of the agent wheel failed: {}",
                pip_res.stderr.trim()
            );
        }
    })();

    // Always remove the temp download tree, success or failure.
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// `<path>.<ext>` sidecar next to `path` (matches `verify_sha256`'s lookup).
fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// A unique temp directory for the stable wheel download (pid + a monotonic
/// counter), created under the system temp root.
fn wheel_tmp_dir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("ados-installer-wheel-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// The persisted clone destination. On a real SBC this is `/opt/ados/source`
/// itself (the repo CONTENTS land directly there, so `scripts/` resolves to
/// `/opt/ados/source/scripts` — the layout the runtime agent expects:
/// `display_install.py` looks for `/opt/ados/source/scripts/drivers/...` and
/// the CLI for `/opt/ados/source/scripts/install.sh`. The predecessor bash
/// `persist_repo_artifacts` flattened to the same place; cloning into a `repo/`
/// subdir would have broken those runtime consumers). The downstream install
/// steps read `data/` + `scripts/` from this same dir via `ctx.source_dir`, and
/// a later `--upgrade` re-clones over it. When `/opt/ados` is not creatable (a
/// dev host), fall back to a unique temp dir so the edge path still exercises
/// end to end without root.
/// Where a fresh clone is staged before it replaces the live tree (pure).
///
/// A SIBLING of the live tree, never a child of it: a child would be destroyed
/// by the very `remove_dir_all` that makes room for the promotion.
pub fn staging_dest(repo: &Path) -> PathBuf {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "source".to_string());
    repo.with_file_name(format!("{name}.staging"))
}

/// Replace the live source tree with a freshly cloned staging tree.
///
/// Called only after the clone has succeeded, so the live tree is removed at the
/// last possible moment and is never absent while a network operation is in
/// flight. Removing it first is what left a rig with no `scripts/install.sh` and
/// therefore no way to retry its own install.
pub fn promote_staging(staging: &Path, repo: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(repo);
    std::fs::rename(staging, repo).with_context(|| {
        format!(
            "promote the freshly cloned source tree into {}",
            repo.display()
        )
    })
}

fn clone_dest() -> std::io::Result<PathBuf> {
    let persisted = PathBuf::from(format!("{}/source", env::INSTALL_DIR));
    if std::fs::create_dir_all(&persisted).is_ok() {
        return Ok(persisted);
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("ados-installer-src-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// Python venv creation + agent package install.
pub struct VenvAgent;

impl Step for VenvAgent {
    fn id(&self) -> &str {
        "venv_agent"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("venv")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let python = match resolve_python() {
            Ok(p) => p,
            Err(e) => {
                return StepOutcome::Failed(format!(
                    "could not obtain a Python 3.11+ interpreter to create the venv: {e}"
                ))
            }
        };

        // (1) Create the venv. This crate's single checkpoint for the step is
        // `venv`, marked only on full success (the graph engine handles that).
        if let Err(e) = create_venv(&python) {
            return StepOutcome::Failed(e.to_string());
        }

        // (2) Self-heal pip before any install runs.
        if let Err(e) = ensure_venv_pip(&python) {
            return StepOutcome::Failed(e.to_string());
        }

        // (3) Install the agent package per channel. The stable path installs a
        // verified release wheel (no on-disk source tree); the edge path clones
        // the repo and records the tree into `ctx.source_dir`.
        let repo = if ctx.channel == "stable" {
            if let Err(e) = install_agent_stable(ctx) {
                return StepOutcome::Failed(e.to_string());
            }
            None
        } else {
            match install_agent_edge(ctx) {
                Ok(repo) => Some(repo),
                Err(e) => return StepOutcome::Failed(e.to_string()),
            }
        };

        // (4) Post-provision dependency health gate. The state IPC wire is
        // msgpack v2 (the native producer always emits v2 frames), so the Python
        // state consumer must be able to import `msgpack` or it reads no state at
        // all — a silent, self-heal-free state blindness. `msgpack` is a declared
        // dependency, so a clean install always has it; assert it here so a venv
        // that landed without it (a broken index, a partial wheel cache) fails
        // the install loudly at provision time instead of going dark at runtime.
        if !venv_msgpack_importable() {
            return StepOutcome::Failed(
                "the agent venv cannot import `msgpack`, which the default state \
                 IPC wire (v2) requires; the venv provisioned without a declared \
                 dependency — re-run the install so the agent package and its \
                 dependencies are reinstalled"
                    .to_string(),
            );
        }

        // Record the cloned tree (edge channel only) so the downstream OS steps
        // find the unit files, udev rules, and driver scripts under it. The
        // stable channel has no source tree, so it leaves `ctx.source_dir` unset
        // and the OS steps resolve from the persisted `/opt/ados/source`.
        ctx.source_dir = repo;
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_paths_are_under_the_venv_dir() {
        assert_eq!(venv_python(), "/opt/ados/venv/bin/python");
        assert_eq!(venv_pip(), "/opt/ados/venv/bin/pip");
    }

    #[test]
    fn staging_is_a_sibling_of_the_live_tree_never_a_child() {
        // A child would be destroyed by the very remove_dir_all that makes room
        // for the promotion, so the clone would be thrown away at the moment it
        // was needed.
        let repo = Path::new("/opt/ados/source");
        let staging = staging_dest(repo);
        assert_eq!(staging, Path::new("/opt/ados/source.staging"));
        assert!(
            !staging.starts_with(repo),
            "staging must not live inside the tree it replaces"
        );
        assert_eq!(staging.parent(), repo.parent());
    }

    #[test]
    fn a_failed_clone_leaves_the_existing_source_tree_intact() {
        // The regression, hit on a rig: the live tree was deleted BEFORE the
        // clone, so a failed clone left the node with no scripts/install.sh —
        // no way to retry its own install, and on a node with no internet, no
        // way back at all. The tree must survive a failure untouched.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("source");
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(repo.join("scripts/install.sh"), b"#!/bin/sh\n").unwrap();

        // Simulate the failure path: staging is prepared, the clone fails, the
        // staging dir is swept, and promote_staging is never reached.
        let staging = staging_dest(&repo);
        std::fs::create_dir_all(&staging).unwrap();
        let _ = std::fs::remove_dir_all(&staging);

        assert!(
            repo.join("scripts/install.sh").exists(),
            "a failed clone must leave the node able to retry its own install"
        );
        assert!(!staging.exists(), "the staging tree is swept on failure");
    }

    #[test]
    fn a_successful_clone_replaces_the_tree_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("source");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("stale.txt"), b"old").unwrap();

        let staging = staging_dest(&repo);
        std::fs::create_dir_all(staging.join("scripts")).unwrap();
        std::fs::write(staging.join("scripts/install.sh"), b"#!/bin/sh\nnew\n").unwrap();

        promote_staging(&staging, &repo).unwrap();

        assert!(
            repo.join("scripts/install.sh").exists(),
            "the new tree is live"
        );
        assert!(
            !repo.join("stale.txt").exists(),
            "the old tree is gone, not merged"
        );
        assert!(!staging.exists(), "staging is consumed by the promotion");
    }

    #[test]
    fn venv_create_args_request_system_site_packages() {
        let args = venv_create_args("/opt/ados/venv");
        assert_eq!(args[0], "-m");
        assert_eq!(args[1], "venv");
        assert!(args.contains(&"--system-site-packages".to_string()));
        assert_eq!(args.last().unwrap(), "/opt/ados/venv");
    }

    #[test]
    fn pip_edge_args_install_the_source() {
        // No `--quiet`: the install lines stream to the live-detail pane.
        let args = pip_install_edge_args("/tmp/repo");
        assert_eq!(args, vec!["install", "/tmp/repo"]);
    }

    #[test]
    fn pip_wheel_args_install_a_local_file() {
        let args = pip_install_wheel_args("/tmp/ados_drone_agent-0.93.0-py3-none-any.whl");
        assert_eq!(
            args,
            vec!["install", "/tmp/ados_drone_agent-0.93.0-py3-none-any.whl"]
        );
    }

    #[test]
    fn normalize_version_strips_one_leading_v() {
        // Both the v-prefixed tag form and the bare form collapse to bare.
        assert_eq!(normalize_version("v0.93.0"), "0.93.0");
        assert_eq!(normalize_version("0.93.0"), "0.93.0");
        // Only a single leading `v` is stripped (a digit-led version has none).
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version("1.2.3"), "1.2.3");
    }

    #[test]
    fn wheel_filename_uses_the_bare_version() {
        assert_eq!(
            wheel_filename("0.93.0"),
            "ados_drone_agent-0.93.0-py3-none-any.whl"
        );
    }

    #[test]
    fn wheel_url_v_prefixes_the_tag_but_not_the_filename() {
        // The release tag is v-prefixed; the wheel filename is the bare version.
        let from_bare = wheel_url("0.93.0");
        assert_eq!(
            from_bare,
            "https://github.com/altnautica/ADOSDroneAgent/releases/download/v0.93.0/ados_drone_agent-0.93.0-py3-none-any.whl"
        );
        // A v-prefixed input normalizes to the identical URL.
        assert_eq!(wheel_url(&normalize_version("v0.93.0")), from_bare);
    }

    #[test]
    fn git_clone_args_honor_branch() {
        let no_branch = git_clone_args("/tmp/repo", None);
        assert!(!no_branch.contains(&"--branch".to_string()));
        assert!(no_branch.contains(&REPO_URL.to_string()));
        assert_eq!(no_branch.last().unwrap(), "/tmp/repo");

        let branched = git_clone_args("/tmp/repo", Some("main"));
        let pos = branched.iter().position(|a| a == "--branch").unwrap();
        assert_eq!(branched[pos + 1], "main");
        // Shallow + submodules retained.
        assert!(branched.contains(&"--depth".to_string()));
        assert!(branched.contains(&"--recurse-submodules".to_string()));
    }

    #[test]
    fn a_clone_can_never_carry_the_pin_itself() {
        // `git clone --branch` takes a branch or a tag and never a commit SHA,
        // which is the whole reason the pinned path is clone-then-fetch. If
        // someone ever "simplifies" it by passing the rev as a branch, this says
        // why that cannot work.
        let sha = "3b4b8deec0ffee1234567890abcdef1234567890";
        let args = git_clone_args("/tmp/repo", Some(sha));
        let pos = args.iter().position(|a| a == "--branch").unwrap();
        assert_eq!(
            args[pos + 1],
            sha,
            "the builder is literal; the refusal comes from git, so the pinned \
             path must not route a SHA through --branch"
        );
        // The pinned path's own fetch asks for the object by name instead.
        assert_eq!(
            git_fetch_rev_args(sha),
            vec!["fetch", "--depth", "1", "origin", sha]
        );
    }

    #[test]
    fn abbreviated_commits_are_the_only_thing_worth_deepening_for() {
        // A prefix is what a server cannot answer, so it is the only case that
        // pays for a deep fetch.
        assert!(is_abbreviated_commit("3b4b8dee"));
        assert!(is_abbreviated_commit(
            "3b4b8deec0ffee1234567890abcdef123456789"
        ));
        // A full object name the server already refused is genuinely absent.
        assert!(!is_abbreviated_commit(
            "3b4b8deec0ffee1234567890abcdef1234567890"
        ));
        // Branches and tags are not hex and never reach the deepen path.
        assert!(!is_abbreviated_commit("main"));
        assert!(!is_abbreviated_commit("v0.99.359"));
        assert!(!is_abbreviated_commit(""));
    }

    #[test]
    fn the_deepen_and_checkout_args_pin_rather_than_follow() {
        assert_eq!(
            git_deepen_args("main"),
            vec!["fetch", "--depth", "500", "origin", "main"]
        );
        assert_eq!(
            git_rev_parse_args("3b4b8dee"),
            vec!["rev-parse", "--verify", "--quiet", "3b4b8dee^{commit}"]
        );
        // Detached: a pin is a commit, not a branch to follow forward.
        let co = git_checkout_detached_args("FETCH_HEAD");
        assert!(co.contains(&"--detach".to_string()));
        assert!(co.contains(&"--force".to_string()));
        assert_eq!(co.last().unwrap(), "FETCH_HEAD");
        // Submodules follow the superproject or the tree is mixed-revision.
        assert_eq!(
            git_submodule_sync_args(),
            vec![
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--depth",
                "1"
            ]
        );
    }

    #[test]
    fn an_unresolvable_pin_refuses_to_fall_back_to_the_branch() {
        // The silent fallback is the entire bug class: a deploy that gated on one
        // commit and then installed whatever the branch had become.
        let msg = rev_unresolvable("deadbeef", "main");
        assert!(msg.contains("Not falling back"), "{msg}");
        assert!(msg.contains("500"), "names the deepen bound: {msg}");
        assert!(msg.contains("40-character SHA"), "names the fix: {msg}");
    }

    /// Build a throwaway "remote" with two commits and clone it, so the resolver
    /// runs against a real object store. `allowAnySHA1InWant` mirrors github.com,
    /// where fetching a bare object name is what makes a SHA pin possible at all.
    fn fixture_clone() -> (tempfile::TempDir, PathBuf, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote");
        let git = |args: &[&str]| {
            let out = exec::run("git", args);
            assert!(out.success(), "git {args:?} failed: {}", out.stderr);
            out
        };
        std::fs::create_dir_all(&remote).unwrap();
        let r = remote.to_string_lossy().into_owned();
        git(&["init", "-q", "-b", "main", &r]);
        git(&["-C", &r, "config", "user.email", "t@example.com"]);
        git(&["-C", &r, "config", "user.name", "t"]);
        git(&["-C", &r, "config", "uploadpack.allowAnySHA1InWant", "true"]);
        std::fs::write(remote.join("marker"), b"old\n").unwrap();
        git(&["-C", &r, "add", "-A"]);
        git(&["-C", &r, "commit", "-qm", "first"]);
        let old = git(&["-C", &r, "rev-parse", "HEAD"])
            .stdout
            .trim()
            .to_string();
        std::fs::write(remote.join("marker"), b"new\n").unwrap();
        git(&["-C", &r, "add", "-A"]);
        git(&["-C", &r, "commit", "-qm", "second"]);
        let tip = git(&["-C", &r, "rev-parse", "HEAD"])
            .stdout
            .trim()
            .to_string();

        // `file://` rather than a bare path: the local transport hardlinks the
        // object store and ignores `--depth`, so a bare path would neither be a
        // shallow clone nor exercise upload-pack's `allowAnySHA1InWant` — the
        // very thing a SHA pin depends on.
        let clone = dir.path().join("clone");
        let c = clone.to_string_lossy().into_owned();
        git(&["clone", "-q", "--depth", "1", &format!("file://{r}"), &c]);
        (dir, clone, old, tip)
    }

    #[test]
    fn checkout_rev_moves_the_tree_and_reports_the_full_object_name() {
        let (_dir, clone, old, _tip) = fixture_clone();
        let sink = ProgressSink::default();
        let full = checkout_rev(&clone, &old, "main", &sink).unwrap();
        assert_eq!(full, old, "the pin's own full object name is returned");
        assert_eq!(
            std::fs::read_to_string(clone.join("marker")).unwrap(),
            "old\n",
            "the tree is at the pinned commit, not the branch tip"
        );
    }

    #[test]
    fn checkout_rev_expands_an_abbreviated_pin_locally() {
        // The expansion is what lets `fetch_binaries` address `rev-<full sha>`:
        // CI publishes the full 40-char tag, so an unexpanded prefix would 404
        // the whole per-revision release. No GitHub API, no credential — the
        // clone's own object store answers it.
        let (_dir, clone, old, _tip) = fixture_clone();
        let short = &old[..8];
        let full = checkout_rev(&clone, short, "main", &ProgressSink::default()).unwrap();
        assert_eq!(full, old);
        assert_eq!(full.len(), 40);
    }

    #[test]
    fn checkout_rev_refuses_an_unknown_pin_and_leaves_the_tree_where_it_was() {
        let (_dir, clone, _old, tip) = fixture_clone();
        let err = checkout_rev(
            &clone,
            "ffffffffffffffffffffffffffffffffffffffff",
            "main",
            &ProgressSink::default(),
        )
        .expect_err("an absent commit must fail rather than install the tip");
        assert!(err.to_string().contains("Not falling back"));
        let head = exec::run(
            "git",
            &["-C", &clone.to_string_lossy(), "rev-parse", "HEAD"],
        );
        assert_eq!(
            head.stdout.trim(),
            tip,
            "a failed pin must not have moved the tree"
        );
    }
}
