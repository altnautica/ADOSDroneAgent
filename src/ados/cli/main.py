"""Minimal public CLI for ADOS Drone Agent."""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

import click

# The status, update, install and uninstall commands live in their own modules;
# every name this module used to define stays importable from here.
from ados.cli._main_install import (  # noqa: F401
    INSTALL_CHECKPOINT_STEPS,
    INSTALLER_PERSISTED_PATH,
    _checkpoint_state,
    _install_resume,
    _install_status,
    _read_install_result,
    _resolve_installer_path,
    install,
)
from ados.cli._main_status import (  # noqa: F401
    _SETUP_FALLBACK_STATUSES,
    IS_MACOS,
    _answered_locally,
    _console_reach_urls,
    _native_status,
    _plain_status,
    _read_native,
    _request,
    _setup_status,
    _viewer_url_from_whep,
    status,
)
from ados.cli._main_uninstall import (  # noqa: F401
    _MACOS_DAEMONS,
    LOCAL_INSTALLER_PATH,
    _macos_ados_home,
    _run_as_root,
    _run_uninstall_linux,
    _uninstall_macos,
    uninstall,
)
from ados.cli._main_update import (  # noqa: F401
    ARMED_STATE_MAX_AGE_S,
    INSTALL_SH_URL,
    REMOTE_VERSION_URL,
    _armed_refusal,
    _installed_version,
    _latest_main_version,
    _run_upgrade,
    _upgrade_profile,
    _vehicle_arm_state,
    update,
)


def _tui_binary() -> str | None:
    """Locate the ados-tui dashboard binary, if installed.

    The live terminal dashboard is the Rust ``ados-tui`` binary. It is
    installed alongside the agent; an override is honoured for development.
    """
    candidates: list[str] = []
    override = os.environ.get("ADOS_TUI_BIN")
    if override:
        candidates.append(override)
    candidates.append("/opt/ados/bin/ados-tui")
    on_path = shutil.which("ados-tui")
    if on_path:
        candidates.append(on_path)
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _print_version(ctx: click.Context, _param: click.Parameter, value: bool) -> None:
    if not value or ctx.resilient_parsing:
        return
    from ados import __version__
    click.echo(__version__)
    ctx.exit()


@click.group(invoke_without_command=True)
@click.option(
    "--version",
    is_flag=True,
    is_eager=True,
    expose_value=False,
    callback=_print_version,
    help="Show the agent version and exit.",
)
@click.pass_context
def cli(ctx: click.Context) -> None:
    """ADOS Drone Agent."""
    if ctx.invoked_subcommand is None:
        # An interactive terminal hands off to the Rust dashboard binary; this
        # process is replaced by it. Without a TTY (or a dashboard binary),
        # fall back to the one-shot plain status.
        if sys.stdin.isatty() and sys.stdout.isatty():
            tui = _tui_binary()
            if tui:
                try:
                    os.execv(tui, [tui])  # replaces this process; does not return
                except OSError:
                    pass
        _plain_status(_setup_status())


@cli.command()
def version() -> None:
    """Print the installed agent version (works when the service is down)."""
    from ados import __version__
    click.echo(__version__)


cli.add_command(status)
cli.add_command(update)
cli.add_command(install)
cli.add_command(uninstall)


# Wire subcommand groups. Done at import time so the entry point in
# pyproject.toml (ados = ados.cli.main:cli) sees the full command tree.
from ados.cli.config import config_group  # noqa: E402
from ados.cli.diag import diag_group  # noqa: E402
from ados.cli.hardware import hardware_group  # noqa: E402
from ados.cli.help import help_command  # noqa: E402
from ados.cli.logs import logs_group  # noqa: E402
from ados.cli.mcp import mcp_group  # noqa: E402
from ados.cli.network import network_group  # noqa: E402
from ados.cli.pair import pair, unpair  # noqa: E402
from ados.cli.plugin import plugin_group  # noqa: E402
from ados.cli.profile import profile_group  # noqa: E402
from ados.cli.radio import radio_group  # noqa: E402
from ados.cli.record import record_group  # noqa: E402
from ados.cli.support import support_bundle  # noqa: E402

# Primitive operator commands stay on the primary help surface. The advanced
# groups keep working (log RCA, service toggles, plugins, …) but are hidden so
# the common path is uncluttered; `ados help` lists the primitives.
cli.add_command(pair)
cli.add_command(unpair)
cli.add_command(help_command)
cli.add_command(logs_group)
cli.add_command(support_bundle)

# The advanced groups: public, documented commands kept off `ados --help`.
# Hidden here means "not on the primary help surface", not "internal", so the
# docs reference exporter (scripts/emit-reference.py) still lists them.
ADVANCED_GROUPS: tuple[click.Group, ...] = (
    config_group,
    diag_group,
    hardware_group,
    mcp_group,
    network_group,
    plugin_group,
    profile_group,
    radio_group,
    record_group,
)

for _group in ADVANCED_GROUPS:
    _group.hidden = True
    cli.add_command(_group)


if __name__ == "__main__":
    cli()
