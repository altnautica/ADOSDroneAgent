"""``ados update``: re-run the canonical installer in upgrade mode."""

from __future__ import annotations

import json
import os
import platform
import re
import shutil
import subprocess
import tempfile

import click
import httpx

from ados.cli import _agent_http
from ados.cli._main_install import _read_install_result
from ados.cli._main_status import _request
from ados.core.profile import _read_profile_conf_value

# How old the vehicle state may be before an upgrade treats the vehicle as
# armed; the same bound as `ados_protocol::armed::ARMED_STATE_MAX_AGE`.
ARMED_STATE_MAX_AGE_S = 3.0


# The canonical install one-liner drives updates too: `ados update` re-runs it
# in upgrade mode, which is the ONE path that actually updates the agent (the
# Rust daemons + the CLI). On Linux it refetches the prebuilt installer and
# re-runs the full chain; on macOS it git-pulls the source and rebuilds. Both
# preserve identity/config. This deliberately replaces the old pip-wheel OTA,
# which only ever updated a Python wheel, not the Rust agent.
INSTALL_SH_URL = "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh"
REMOTE_VERSION_URL = (
    "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/src/ados/__init__.py"
)


def _installed_version() -> str:
    """The version this agent reports, preferring the running agent, then the
    recorded install result, then the CLI package."""
    try:
        data = _request("GET", "/api/version", timeout=4.0)
        v = data.get("version") or data.get("agent_version")
        if isinstance(v, str) and v:
            return v
    except click.ClickException:
        pass
    result = _read_install_result()
    if result and isinstance(result.get("version"), str):
        return str(result["version"])
    try:
        from ados import __version__

        return __version__
    except Exception:  # noqa: BLE001 - version display only, never fatal
        return "unknown"


def _latest_main_version() -> str | None:
    """The `__version__` on the tip of `main`, or None if it can't be fetched."""
    try:
        with httpx.Client(timeout=15.0, follow_redirects=True) as client:
            resp = client.get(REMOTE_VERSION_URL)
            resp.raise_for_status()
        match = re.search(r'__version__\s*=\s*"([^"]+)"', resp.text)
        return match.group(1) if match else None
    except (httpx.HTTPError, ValueError):
        return None


def _upgrade_profile(override: str | None) -> str | None:
    """The profile an upgrade should pin, or ``None`` when it cannot be known.

    An upgrade must never be allowed to *change* what a box is. The installer
    resolves the profile itself when none is passed, and a gap in that
    resolution has re-profiled a live ground station to ``drone`` and left it
    in a reboot loop that needed a reflash to recover. The box already knows
    what it is — ``/etc/ados/profile.conf`` is written by both install.sh and
    the wizard — so an upgrade states it explicitly rather than relying on a
    default being right.

    An explicit ``--profile`` wins, for the one legitimate case: deliberately
    converting a box from one role to another.
    """
    if override:
        return override
    return _read_profile_conf_value()


def _vehicle_arm_state() -> tuple[str, str | None]:
    """``(arm state, profile)`` from the router state the agent reports.

    The state is ``"disarmed"`` only when a live link says so; a missing,
    unreadable or stale state (older than ``ARMED_STATE_MAX_AGE_S``) is
    ``"unknown"``, which an upgrade treats as armed.
    """
    try:
        response = _agent_http.call("GET", "/api/status/full", timeout=4.0)
    except click.ClickException:
        return "unknown", None
    body = response.body if response.ok and isinstance(response.body, dict) else {}
    profile = body.get("profile") if isinstance(body.get("profile"), str) else None
    age = body.get("heartbeatAgeS")
    fresh = (
        body.get("mavlinkAlive") is True
        and isinstance(age, (int, float))
        and age <= ARMED_STATE_MAX_AGE_S
    )
    telemetry = body.get("telemetry")
    armed = telemetry.get("armed") if isinstance(telemetry, dict) else None
    if not fresh or not isinstance(armed, bool):
        return "unknown", profile
    return ("armed" if armed else "disarmed"), profile


def _armed_refusal(profile: str | None) -> str | None:
    """Why an upgrade must not run now, or ``None`` when it may.

    An upgrade restarts the MAVLink, radio and video services. On a node that
    carries or links a vehicle (drone, ground station) it is refused while the
    vehicle is armed or its arm state is unknown or stale.
    """
    state, reported = _vehicle_arm_state()
    resolved = (reported or profile or "").replace("-", "_")
    if resolved not in ("drone", "ground_station") or state == "disarmed":
        return None
    reason = "armed" if state == "armed" else "of unknown arm state (no fresh vehicle state)"
    return (
        f"Refusing to update: the vehicle is {reason}. An update restarts the "
        "MAVLink, radio and video services. Disarm first, or pass --force-armed."
    )


def _run_upgrade(profile: str | None = None, *, force_armed: bool = False) -> None:
    """Fetch the canonical install.sh (latest main) and run it in upgrade mode.

    Linux needs root (the systemd install writes under /opt and /etc), so
    elevate with sudo when not already root; macOS installs per-user (build
    from source), so no sudo. The installer runs inline so its live progress
    stays attached to this terminal.

    ``profile`` is passed through to the installer when known, so an upgrade
    cannot silently re-profile the box; ``force_armed`` passes the operator's
    armed override to the installer's own preflight.
    """
    try:
        with httpx.Client(timeout=30.0, follow_redirects=True) as client:
            resp = client.get(INSTALL_SH_URL)
            resp.raise_for_status()
    except httpx.HTTPError as exc:
        raise click.ClickException(f"Could not fetch the installer: {exc}") from exc

    fd, script = tempfile.mkstemp(suffix="-ados-install.sh")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(resp.text)
        argv = ["bash", script, "--upgrade"]
        if profile:
            argv += ["--profile", profile]
        if force_armed:
            argv.append("--force-armed")
        if platform.system() == "Linux" and os.geteuid() != 0:
            if shutil.which("sudo") is None:
                raise click.ClickException(
                    "Updating needs root on Linux. Re-run as: sudo ados update"
                )
            argv = ["sudo", *argv]
        try:
            completed = subprocess.run(argv, check=False)  # noqa: S603
        except OSError as exc:
            raise click.ClickException(f"Failed to launch the installer: {exc}") from exc
    finally:
        try:
            os.unlink(script)
        except OSError:
            pass

    if completed.returncode != 0:
        raise click.ClickException(f"Update finished with exit code {completed.returncode}.")


@click.command()
@click.option("--check-only", is_flag=True, help="Report the current + latest version, don't install.")
@click.option("--yes", "-y", is_flag=True, help="Update without an interactive prompt.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
@click.option(
    "--profile",
    "profile_override",
    type=click.Choice(["drone", "ground-station", "workstation", "compute"]),
    default=None,
    help="Override the profile to install. Defaults to whatever this box already is.",
)
@click.option(
    "--force-armed",
    is_flag=True,
    help=(
        "Override the armed refusal (here and in the installer): update even while "
        "the vehicle is armed or its arm state is unknown. Can drop the link in flight."
    ),
)
def update(
    check_only: bool,
    yes: bool,
    as_json: bool,
    profile_override: str | None,
    force_armed: bool,
) -> None:
    """Update the agent to the latest and restart it."""
    current = _installed_version()
    latest = _latest_main_version()
    available = bool(latest and latest != current)

    if as_json:
        click.echo(
            json.dumps(
                {
                    "current_version": current,
                    "latest_version": latest,
                    "update_available": available,
                },
                indent=2,
            )
        )
        return

    click.echo(f"Current version: {current}")
    if latest:
        click.echo(f"Latest (main):   {latest}")
    else:
        click.echo("Latest version:  could not check — updating to the latest main anyway.")

    if check_only:
        return

    if latest is not None and not available:
        click.echo("Already up to date.")
        return

    profile = _upgrade_profile(profile_override)
    if not force_armed:
        refusal = _armed_refusal(profile)
        if refusal:
            raise click.ClickException(refusal)

    if not yes:
        click.confirm("Update the agent to the latest now?", abort=True)

    if profile:
        click.echo(f"Profile:         {profile} (pinned, so the upgrade cannot change it)")
    else:
        click.echo(
            "Profile:         unknown — /etc/ados/profile.conf is missing, so the "
            "installer will resolve it. Pass --profile to be certain."
        )
    click.echo("Updating — this rebuilds and restarts the agent…")
    _run_upgrade(profile, force_armed=force_armed)
    click.echo("Update complete.")
