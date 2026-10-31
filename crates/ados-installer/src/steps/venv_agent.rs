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
//!      stable — download the release wheel and deploy bundle for `--version`,
//!      verify each against its SHA256 and its minisign signature (both
//!      mandatory), install the dependencies from the hashed lock inside the
//!      signed bundle, `pip install --no-deps <wheel>`, and unpack the bundle
//!      into the persisted source tree the OS steps read their unit files from
//!
//! The venv-path + pip-args + wheel-URL + git-args builders are pure so a unit
//! test exercises them without a real interpreter or the network.

use std::path::Path;

use ados_term::exec;
use ados_term::progress::ProgressSink;

use crate::ctx::Ctx;
use crate::env;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::rollback::snapshots::{begin_run_record, ROLLBACK_DIR, RUN_RECORD};
use crate::ui::activity;

mod args;
mod edge;
mod stable;
mod staging;

pub use args::{
    bundle_filename, git_checkout_detached_args, git_clone_args, git_deepen_args,
    git_fetch_rev_args, git_rev_parse_args, git_submodule_sync_args, is_abbreviated_commit,
    normalize_version, pip_install_edge_args, pip_install_lock_args, pip_install_wheel_args,
    release_asset_url, rev_unresolvable, venv_create_args, venv_pip, venv_python, wheel_filename,
    wheel_url, BUNDLE_LOCK_MEMBER,
};
use edge::install_agent_edge;
use stable::install_agent_stable;
pub use stable::unpack_bundle;
pub use staging::{promote_staging, staging_dest};

mod retain;

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

        // Start this run's rollback record, then record the retained wheel of
        // the version this run replaces, so a failed install can reinstall it.
        let record = Path::new(RUN_RECORD);
        if let Err(e) = begin_run_record(record) {
            tracing::warn!(error = %e, "rollback record not started");
        }
        let previous = env::installed_agent_version();
        retain::record_previous(Path::new(ROLLBACK_DIR), record, previous.as_deref());

        // (3) Install the agent package per channel. The stable path installs a
        // signature-verified release wheel and unpacks the release's deploy
        // bundle; the edge path clones the repo. Both return the source tree.
        let repo = if ctx.channel == "stable" {
            install_agent_stable(ctx)
        } else {
            install_agent_edge(ctx)
        };
        let repo = match repo {
            Ok(repo) => repo,
            Err(e) => return StepOutcome::Failed(e.to_string()),
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

        // Keep the installed package as a wheel for the next upgrade's
        // rollback. The stable path already kept its verified release wheel.
        // A retention failure never fails the install; it only means the next
        // upgrade has no wheel to restore.
        let rollback_dir = Path::new(ROLLBACK_DIR);
        if ctx.channel != "stable" {
            if let Err(e) = retain::build_and_keep(&venv_pip(), &repo, rollback_dir) {
                tracing::warn!(error = %e, "could not keep the agent wheel for rollback");
            }
        }
        let current = env::installed_agent_version();
        let keep: Vec<&str> = [previous.as_deref(), current.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        retain::prune(rollback_dir, &keep);

        // Record the source tree so the downstream OS steps find the unit files,
        // udev rules, and driver scripts under it.
        ctx.source_dir = Some(repo);
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests;
