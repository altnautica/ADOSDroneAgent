//! Pure argv and URL builders for the venv, pip and git invocations.

use crate::env;
use crate::steps::fetch_binaries;

use super::REPO_URL;

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
/// already-downloaded, already-verified wheel from disk. `--no-deps`: its
/// dependencies come only from the signed hash lock ([`pip_install_lock_args`]),
/// never from a live index resolve.
pub fn pip_install_wheel_args(wheel_path: &str) -> Vec<String> {
    vec![
        "install".to_string(),
        "--no-deps".to_string(),
        wheel_path.to_string(),
    ]
}

/// The hashed dependency lock the release ships inside the signed deploy
/// bundle, relative to the bundle's root.
pub const BUNDLE_LOCK_MEMBER: &str = "repo/requirements.lock";

/// Build the `pip install` args for the stable channel's dependencies (pure):
/// every requirement pinned and hash-checked from the signed lock, nothing
/// resolved beyond it.
pub fn pip_install_lock_args(lock_path: &str) -> Vec<String> {
    vec![
        "install".to_string(),
        "--require-hashes".to_string(),
        "--no-deps".to_string(),
        "-r".to_string(),
        lock_path.to_string(),
    ]
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
    release_asset_url(version, &wheel_filename(version))
}

/// The deploy-bundle asset name for a release: every install asset that lives
/// outside the wheel (`scripts/`, `data/`, the vendored radio source), staged
/// under a top-level `repo/` directory. `version` is the bare form.
pub fn bundle_filename(version: &str) -> String {
    format!("ados-drone-agent-deploy-{version}.tar.gz")
}

/// The URL of one asset in the `v<version>` release (pure apart from the
/// test-only release-base override the binary fetch honours too, so one
/// override points both halves of a stable install at the same fake release).
pub fn release_asset_url(version: &str, name: &str) -> String {
    format!(
        "{}/{}/{name}",
        fetch_binaries::release_base(),
        fetch_binaries::version_release_tag(version)
    )
}

/// Global options that make a networked git command abandon a transfer moving
/// under 1 KB/s for 60 s, instead of waiting on it forever. git has no stall
/// bound of its own, so without these a clone that stops mid-transfer on a
/// flaky link parks the whole install; it is the same stall bound every curl
/// fetch in the installer carries (`net::STALL_WINDOW_SECS`).
pub(super) const GIT_STALL_BOUND: [&str; 4] = [
    "-c",
    "http.lowSpeedLimit=1024",
    "-c",
    "http.lowSpeedTime=60",
];

/// A networked git invocation: the stall bound, then `args`.
fn stall_bounded(args: &[&str]) -> Vec<String> {
    GIT_STALL_BOUND
        .iter()
        .chain(args)
        .map(|s| s.to_string())
        .collect()
}

/// Build the `git clone` args for the edge channel (pure). Honors an optional
/// branch; shallow + submodules, matching `git_clone_retry`.
pub fn git_clone_args(dest: &str, branch: Option<&str>) -> Vec<String> {
    let mut args = stall_bounded(&[
        "clone",
        "--depth",
        "1",
        "--recurse-submodules",
        "--shallow-submodules",
        "--progress",
    ]);
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
pub(super) const DEFAULT_BRANCH: &str = "main";

/// How far back an abbreviated commit is chased when the server refuses the
/// prefix. Bounded on purpose: a commit not in the last 500 of the branch is
/// reported, not hunted. Matches the bootstrap's macOS path.
const REV_DEEPEN_DEPTH: &str = "500";

/// Build the `git fetch` args that pull ONE revision (pure). Depth 1 because a
/// pin needs the commit, not its history. `--branch` cannot do this job: it
/// takes a branch or a tag and never a commit SHA, which is why the pinned path
/// is a clone followed by a fetch rather than a different clone.
pub fn git_fetch_rev_args(rev: &str) -> Vec<String> {
    stall_bounded(&["fetch", "--depth", "1", "origin", rev])
}

/// Build the `git fetch` args that deepen `branch` enough to expand an
/// abbreviated commit locally (pure).
pub fn git_deepen_args(branch: &str) -> Vec<String> {
    stall_bounded(&["fetch", "--depth", REV_DEEPEN_DEPTH, "origin", branch])
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
    stall_bounded(&[
        "submodule",
        "update",
        "--init",
        "--recursive",
        "--depth",
        "1",
    ])
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
