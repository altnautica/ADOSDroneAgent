//! The edge-channel install: git clone/checkout of a branch or rev, then an editable pip install.

use std::path::{Path, PathBuf};

use ados_term::exec;
use ados_term::progress::ProgressSink;

use crate::ctx::Ctx;
use crate::env;

use super::args::{
    git_checkout_detached_args, git_clone_args, git_deepen_args, git_fetch_rev_args,
    git_rev_parse_args, git_submodule_sync_args, is_abbreviated_commit, pip_install_edge_args,
    rev_unresolvable, venv_pip, DEFAULT_BRANCH,
};
use super::staging::{promote_staging, staging_dest};
use super::{on_git_line, on_pip_line};

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
pub(super) fn checkout_rev(
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
/// `scripts/drivers/*` from it. We do NOT delete the clone: the tree stays
/// at `/opt/ados/source` for the runtime display step and a later
/// `--upgrade`; a reinstall re-clones over it after wiping.
///
/// Takes `&mut Ctx` for one reason: a pinned install writes the EXPANDED object
/// name back into `ctx.rev`, which is what lets the later binary fetch address
/// `rev-<full sha>` without asking a server to expand a prefix it cannot.
pub(super) fn install_agent_edge(ctx: &mut Ctx) -> anyhow::Result<PathBuf> {
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
pub(super) fn clone_dest() -> std::io::Result<PathBuf> {
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
