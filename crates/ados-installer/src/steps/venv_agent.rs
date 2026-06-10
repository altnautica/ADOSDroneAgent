//! Venv + agent package: create the Python virtualenv and install the agent
//! package into it. Required. Checkpoint `venv`.
//!
//! Ports the venv + agent-install portion of `scripts/install.d/13-main.sh`
//! (fresh-install path) plus `ensure_venv_pip` from 14-orchestration.sh:
//!   1. `python3 -m venv --system-site-packages /opt/ados/venv`
//!   2. self-heal a rotted pip (probe `pip --version`, recreate the venv on
//!      failure)
//!   3. install the agent package per channel:
//!      edge   — git clone the repo (honoring --branch) + `pip install <repo>`
//!      stable — pip-install the verified wheel (TODO: bails clearly for now)
//!
//! The venv-path + pip-args builders are pure so a unit test exercises them
//! without a real interpreter.

use std::path::PathBuf;

use crate::ctx::Ctx;
use crate::env;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// The agent's git repo URL (edge channel clones from here, honoring --branch).
const REPO_URL: &str = "https://github.com/altnautica/ADOSDroneAgent.git";

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
/// is the local cloned repo path (preferred) or a `git+<url>` spec.
pub fn pip_install_edge_args(source: &str) -> Vec<String> {
    vec![
        "install".to_string(),
        source.to_string(),
        "--quiet".to_string(),
    ]
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
        "--quiet".to_string(),
    ];
    if let Some(b) = branch {
        args.push("--branch".to_string());
        args.push(b.to_string());
    }
    args.push(REPO_URL.to_string());
    args.push(dest.to_string());
    args
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
/// The state IPC wire defaults to length-prefixed msgpack (v2): the env file
/// sets `ADOS_STATE_IPC_MSGPACK=1` and the native state producer emits v2
/// frames. The Python state consumer needs `msgpack` to decode those frames; if
/// the module is absent it cannot read state at all. `msgpack` is a declared
/// agent dependency, so a clean `pip install` of the agent package always pulls
/// it in — but a venv rebuilt off a broken index or a partial wheel cache can
/// silently land without it. This probe is the post-provision assertion that
/// turns that silent gap into a loud install failure.
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

/// Install the agent package on the edge channel: clone the repo (honoring
/// --branch) into a PERSISTED dir, then `pip install <repo>`. Returns the
/// cloned repo path so the caller can record it into `ctx.source_dir` — the
/// downstream `systemd` / `config_identity` / `dkms` steps read `data/systemd`,
/// `data/udev`, and `scripts/drivers/*` from it. We do NOT delete the clone
/// (mirrors the bash installer persisting the tree to `/opt/ados/source`); a
/// reinstall re-clones over it after wiping.
fn install_agent_edge(ctx: &Ctx) -> anyhow::Result<PathBuf> {
    let repo = clone_dest()?;
    let repo_s = repo.to_string_lossy().into_owned();

    // A reinstall must start from a clean tree so `git clone` does not refuse
    // a non-empty destination. Idempotent: a missing dir is a no-op.
    let _ = std::fs::remove_dir_all(&repo);

    let clone = git_clone_args(&repo_s, ctx.args.branch.as_deref());
    let clone_argv: Vec<&str> = clone.iter().map(String::as_str).collect();
    let clone_res = exec::run("git", &clone_argv);
    if !clone_res.success() {
        if !clone_res.spawned {
            anyhow::bail!("git is not installed");
        }
        anyhow::bail!("git clone failed: {}", clone_res.stderr.trim());
    }

    let pip = pip_install_edge_args(&repo_s);
    let pip_argv: Vec<&str> = pip.iter().map(String::as_str).collect();
    let pip_res = exec::run(&venv_pip(), &pip_argv);
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
        let python = match super::deps::find_python() {
            Some(p) => p,
            None => {
                return StepOutcome::Failed(
                    "no Python 3.11+ interpreter available to create the venv".to_string(),
                )
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

        // (3) Install the agent package per channel.
        if ctx.channel == "stable" {
            // The stable path pip-installs a verified wheel; that fetch+verify
            // wiring lands with the channel work. Bail clearly rather than
            // silently doing the wrong thing.
            return StepOutcome::Failed(
                "stable channel agent install is not yet wired; use --channel edge".to_string(),
            );
        }

        let repo = match install_agent_edge(ctx) {
            Ok(repo) => repo,
            Err(e) => return StepOutcome::Failed(e.to_string()),
        };

        // (4) Post-provision dependency health gate. The state IPC wire defaults
        // to msgpack v2 (the env file sets ADOS_STATE_IPC_MSGPACK=1 and the
        // native producer emits v2 frames), so the Python state consumer must be
        // able to import `msgpack` or it reads no state at all — a silent,
        // self-heal-free state blindness. `msgpack` is a declared dependency, so
        // a clean install always has it; assert it here so a venv that landed
        // without it (a broken index, a partial wheel cache) fails the install
        // loudly at provision time instead of going dark at runtime.
        if !venv_msgpack_importable() {
            return StepOutcome::Failed(
                "the agent venv cannot import `msgpack`, which the default state \
                 IPC wire (v2) requires; the venv provisioned without a declared \
                 dependency — re-run the install so the agent package and its \
                 dependencies are reinstalled"
                    .to_string(),
            );
        }

        // Record the cloned tree so the downstream OS steps find the unit files,
        // udev rules, and driver scripts under it.
        ctx.source_dir = Some(repo);
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
    fn venv_create_args_request_system_site_packages() {
        let args = venv_create_args("/opt/ados/venv");
        assert_eq!(args[0], "-m");
        assert_eq!(args[1], "venv");
        assert!(args.contains(&"--system-site-packages".to_string()));
        assert_eq!(args.last().unwrap(), "/opt/ados/venv");
    }

    #[test]
    fn pip_edge_args_install_quietly() {
        let args = pip_install_edge_args("/tmp/repo");
        assert_eq!(args, vec!["install", "/tmp/repo", "--quiet"]);
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
}
