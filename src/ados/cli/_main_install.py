"""``ados install``: inspect or resume the on-disk install."""

from __future__ import annotations

import json
import os
import platform
import subprocess
from pathlib import Path
from typing import Any

import click

from ados.core.paths import INSTALL_CHECKPOINT_DIR, INSTALL_RESULT

# Install orchestration contract paths come from `ados.core.paths`, which
# resolves them per platform: the Linux FHS `/var/lib/ados` (the same literal
# `crates/ados-installer/src/env.rs` STATE_DIR carries, since the installer only
# ever runs on a target) or `~/.ados` on a macOS workstation. Hardcoding the
# Linux paths here made `ados install --status` report a path that cannot exist
# on macOS, and read nothing on a box where the installer had recorded a result.
# The checkpoint step list stays here because it is a CLI display contract, not a
# path: the REQUIRED steps the full-agent install records a checkpoint for, in
# install order, used by `ados install --status` to show done vs missing.
INSTALL_CHECKPOINT_STEPS = (
    "deps",
    "venv",
    "systemd",
    "global-symlinks",
)
# Canonical persisted installer path written by the install's
# persist_repo_artifacts step; falls back to the global `ados`-adjacent
# script when absent. --resume re-invokes this in resume mode.
INSTALLER_PERSISTED_PATH = Path("/opt/ados/source/scripts/install.sh")


def _read_install_result() -> dict[str, Any] | None:
    try:
        if INSTALL_RESULT.exists():
            data = json.loads(INSTALL_RESULT.read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else None
    except (OSError, ValueError, json.JSONDecodeError):
        return None
    return None


def _checkpoint_state() -> tuple[list[str], list[str]]:
    """Return (done, missing) checkpoint step names, in install order."""
    done: list[str] = []
    missing: list[str] = []
    for step in INSTALL_CHECKPOINT_STEPS:
        marker = INSTALL_CHECKPOINT_DIR / f"{step}.done"
        if marker.exists():
            done.append(step)
        else:
            missing.append(step)
    return done, missing


@click.command(name="install", hidden=True)
@click.option(
    "--status",
    "show_status",
    is_flag=True,
    default=False,
    help="Show the last install result and which checkpoints are done/missing.",
)
@click.option(
    "--resume",
    "do_resume",
    is_flag=True,
    default=False,
    help="Re-run the installer to finish any missing steps (resume a partial install).",
)
@click.option("--json", "as_json", is_flag=True, default=False, help="Output JSON for scripts.")
def install(show_status: bool, do_resume: bool, as_json: bool) -> None:
    """Inspect or resume the on-disk install.

    With --status, print the last install-result.json and the per-step
    checkpoint state. With --resume, re-run the installer in resume mode so
    a half-finished install (for example, one interrupted by a dropped SSH
    session) completes the missing steps. The installer is idempotent, so a
    resume on a healthy box is a fast no-op.
    """
    if do_resume:
        _install_resume()
        return

    # Default to --status when no action flag is given.
    _install_status(as_json=as_json)


def _install_status(*, as_json: bool) -> None:
    result = _read_install_result()
    done, missing = _checkpoint_state()

    if as_json:
        click.echo(
            json.dumps(
                {
                    "result": result,
                    "checkpoints": {"done": done, "missing": missing},
                },
                indent=2,
            )
        )
        return

    if result is None:
        click.echo(f"No install result recorded at {INSTALL_RESULT}.")
        click.echo("The installer has not finished a run on this box yet.")
    else:
        status = str(result.get("status", "unknown"))
        click.echo(f"Install status: {status}")
        click.echo(f"  Version:  {result.get('version', 'unknown')}")
        click.echo(f"  Profile:  {result.get('profile', 'unknown')}")
        click.echo(f"  Board:    {result.get('board', 'unknown')}")
        click.echo(f"  Kernel:   {result.get('kernelRelease', 'unknown')}")
        wfb = result.get("wfbModuleSource", "")
        click.echo(f"  WFB driver: {wfb or 'not installed'}")
        req = result.get("requiredFailures") or []
        failed = result.get("failedSteps") or []
        if req:
            click.echo(f"  Required failures: {', '.join(req)}")
        if failed:
            click.echo(f"  Failed steps:      {', '.join(failed)}")
        click.echo(f"  Recorded: {result.get('ts', 'unknown')}")

    click.echo("")
    click.echo(f"Checkpoints done ({len(done)}): {', '.join(done) or '<none>'}")
    click.echo(f"Checkpoints missing ({len(missing)}): {', '.join(missing) or '<none>'}")

    if missing or (result is not None and result.get("status") == "failed"):
        click.echo("")
        click.echo("Run 'sudo ados install --resume' to finish the missing steps.")


def _install_resume() -> None:
    if platform.system() != "Linux":
        raise click.ClickException("Resume is only supported on Linux installs.")
    if os.geteuid() != 0:
        raise click.ClickException("Resume must run as root: sudo ados install --resume")

    installer = _resolve_installer_path()
    if installer is None:
        raise click.ClickException(
            "Could not find the installer on disk. Re-run the install one-liner "
            "to recover this box."
        )

    click.echo(f"Resuming install via {installer} ...")
    # A plain re-run resumes by design: the completeness gate routes an
    # incomplete-but-present agent back through the (idempotent) install
    # body and skips finished checkpoints. The installer runs inline, so the
    # resume stays attached to this terminal and the operator sees progress
    # directly.
    env = dict(os.environ)
    try:
        completed = subprocess.run(  # noqa: S603
            ["/usr/bin/env", "bash", str(installer)],
            env=env,
            check=False,
        )
    except OSError as exc:
        raise click.ClickException(f"Failed to launch installer: {exc}") from exc

    if completed.returncode != 0:
        raise click.ClickException(
            f"Resume finished with exit code {completed.returncode}. "
            "Run 'ados install --status' for details."
        )
    click.echo("Resume complete.")


def _resolve_installer_path() -> Path | None:
    """Find the install.sh to re-run for a resume.

    Prefer the persisted copy under /opt/ados/source (written by the
    install's persist step). Fall back to a checkout adjacent to the
    running package source so a dev/editable install can resume too.
    """
    if INSTALLER_PERSISTED_PATH.exists():
        return INSTALLER_PERSISTED_PATH
    # Dev fallback: <repo>/scripts/install.sh relative to this module.
    try:
        repo_installer = Path(__file__).resolve().parents[3] / "scripts" / "install.sh"
        if repo_installer.exists():
            return repo_installer
    except (OSError, IndexError):
        pass
    return None
