"""``ados uninstall``: remove the agent through the installer's uninstall mode."""

from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import click
import httpx

from ados.cli import _ansi
from ados.cli._main_update import INSTALL_SH_URL

# The copy of the Rust installer a successful install keeps on the box
# (`env::INSTALLED_INSTALLER` in crates/ados-installer), so uninstall runs
# offline through the one uninstall path.
LOCAL_INSTALLER_PATH = Path("/opt/ados/bin/ados-installer")


def _run_as_root(argv: list[str]) -> None:
    """Run ``argv`` inline (sudo-elevated when not root); a non-zero exit fails."""
    if os.geteuid() != 0:
        if shutil.which("sudo") is None:
            raise click.ClickException(
                "Uninstall needs root on Linux. Re-run as: sudo ados uninstall"
            )
        argv = ["sudo", *argv]
    try:
        completed = subprocess.run(argv, check=False)  # noqa: S603
    except OSError as exc:
        raise click.ClickException(f"Failed to launch the installer: {exc}") from exc
    if completed.returncode != 0:
        raise click.ClickException(
            f"Uninstall finished with exit code {completed.returncode}."
        )


def _run_uninstall_linux(*, purge: bool, yes: bool) -> None:
    """Confirm, then run the Rust installer's ``--uninstall`` mode.

    The installer owns the removal list and renders the same full-screen
    progress as the install, so it is the only uninstall path. A successful
    install keeps a copy at ``LOCAL_INSTALLER_PATH``, which runs offline; a box
    without that copy fetches the canonical install.sh, which downloads the
    installer and passes the flags through. Raises ``click.Abort`` if the
    operator declines the confirmation.
    """
    if not yes:
        click.confirm("Uninstall the ADOS Drone Agent from this device?", abort=True)
    # `--force` is how the installer requests a config purge on uninstall.
    flags = ["--uninstall", *(["--force"] if purge else [])]
    if LOCAL_INSTALLER_PATH.is_file() and os.access(LOCAL_INSTALLER_PATH, os.X_OK):
        _run_as_root([str(LOCAL_INSTALLER_PATH), *flags])
        return
    try:
        with httpx.Client(timeout=30.0, follow_redirects=True) as client:
            resp = client.get(INSTALL_SH_URL)
            resp.raise_for_status()
    except httpx.HTTPError as exc:
        raise click.ClickException(
            "The installer is not on this device and could not be downloaded "
            f"({exc}). Connect to the internet and run 'sudo ados uninstall' again."
        ) from exc

    fd, script = tempfile.mkstemp(suffix="-ados-uninstall.sh")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(resp.text)
        _run_as_root(["bash", script, *flags])
    finally:
        try:
            os.unlink(script)
        except OSError:
            pass


@click.command()
@click.option("--purge", is_flag=True, default=False, help="Remove config as well.")
@click.option("--yes", "-y", is_flag=True, default=False, help="Skip confirmation prompt.")
def uninstall(purge: bool, yes: bool) -> None:
    """Uninstall ADOS Drone Agent from this system."""
    is_linux = platform.system() == "Linux"
    is_mac = platform.system() == "Darwin"
    if not is_linux and not is_mac:
        raise click.ClickException(f"Unsupported platform: {platform.system()}")

    if is_mac:
        _uninstall_macos(purge=purge, yes=yes)
        return
    _run_uninstall_linux(purge=purge, yes=yes)


# The macOS workstation daemons registered as per-user LaunchAgents by the
# installer (macos.rs). `ados-tui` is installed but is not a daemon, so it has no
# LaunchAgent to boot out. The reverse-DNS labels are `co.ados.<tail>`.
_MACOS_DAEMONS = ("supervisor", "control", "cloud", "logd")


def _macos_ados_home() -> Path:
    """The per-user install root the macOS installer wrote (``$HOME/.ados``),
    honouring ``ADOS_HOME`` the same way the installer + paths module do."""
    override = os.environ.get("ADOS_HOME")
    if override:
        return Path(override)
    return Path.home() / ".ados"


def _uninstall_macos(*, purge: bool, yes: bool) -> None:
    """Tear down a macOS workstation node: boot every LaunchAgent out of the
    user's GUI domain, remove the plists, drop ``$HOME/.ados`` when purging, and
    (best-effort) pip-uninstall the CLI if it was pip-installed. Mirrors the
    installer's ``macos.rs`` uninstall so nothing is left running."""
    ados_home = _macos_ados_home()
    launch_agents = Path.home() / "Library" / "LaunchAgents"
    uid = os.getuid()

    if not yes:
        click.confirm("Uninstall ADOS from this Mac?", abort=True)
        # Mirror the Linux prompt: offer to purge identity + config when the
        # operator did not pass an explicit flag.
        if not purge and ados_home.exists():
            click.echo(f"  Config + identity live under {ados_home}.")
            click.echo("  Keep them for a re-install, or purge for a clean slate.")
            purge = click.confirm("Also remove ~/.ados?", default=False)

    pkg = "ados-drone-agent"

    def _stop_agents() -> str:
        stopped = 0
        for tail in _MACOS_DAEMONS:
            label = f"co.ados.{tail}"
            target = f"gui/{uid}/{label}"
            # Only boot out a genuinely-loaded job so a stale one does not error.
            probe = subprocess.run(
                ["launchctl", "print", target], capture_output=True, text=True
            )
            if probe.returncode == 0:
                subprocess.run(
                    ["launchctl", "bootout", target], capture_output=True, text=True
                )
                stopped += 1
        return f"{stopped} LaunchAgent{'s' if stopped != 1 else ''}"

    def _remove_plists() -> str:
        removed = 0
        if launch_agents.is_dir():
            for plist in sorted(launch_agents.glob("co.ados.*.plist")):
                try:
                    plist.unlink()
                    removed += 1
                except OSError:
                    pass
        return f"{removed} plist{'s' if removed != 1 else ''}"

    def _pip_remove() -> str:
        installer = "pip"
        for candidate, probe in (
            ("pipx", ["pipx", "list", "--short"]),
            ("uv", ["uv", "tool", "list"]),
        ):
            try:
                probed = subprocess.run(probe, capture_output=True, text=True, timeout=10)
                if probed.returncode == 0 and pkg in probed.stdout:
                    installer = candidate
                    break
            except FileNotFoundError:
                pass
        cmd = {
            "pipx": ["pipx", "uninstall", pkg],
            "uv": ["uv", "tool", "uninstall", pkg],
            "pip": [sys.executable, "-m", "pip", "uninstall", "-y", pkg],
        }[installer]
        result = subprocess.run(cmd, capture_output=True, text=True)
        # Not installed via a package manager (the installer builds from source
        # and does not pip-install) is fine — the node is already torn down.
        if result.returncode != 0:
            return "not package-managed (skipped)"
        return f"via {installer}"

    def _purge_home() -> str:
        if ados_home.exists():
            shutil.rmtree(ados_home, ignore_errors=True)
        return str(ados_home)

    steps: list[_ansi.Step] = [
        ("Stop ADOS LaunchAgents", _stop_agents),
        ("Remove LaunchAgent plists", _remove_plists),
        ("Remove ados CLI package", _pip_remove),
    ]
    if purge:
        steps.append(("Purge ~/.ados", _purge_home))

    theme = _ansi.detect_theme()
    results = _ansi.run_steps(
        theme, steps, title="Uninstalling ADOS", interactive=sys.stderr.isatty()
    )
    ok = all(r.ok for r in results)
    done = sum(1 for r in results if r.ok)
    glyph = theme.glyph_ok() if ok else theme.glyph_fail()
    summary = [
        f"{glyph} ADOS Workstation {'removed' if ok else 'removal finished with warnings'}",
        f"{done}/{len(results)} steps",
    ]
    if not purge:
        summary.append(f"kept: {ados_home}  (--purge to remove)")
    _ansi.print_card(theme, ok, summary)
    if not ok:
        raise click.ClickException("uninstall finished with warnings")
