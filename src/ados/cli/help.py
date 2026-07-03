"""``ados help`` — a curated overview of the everyday commands.

The primary surface an operator sees. The advanced command groups still work
(``ados rust``, ``ados plugin``, ``ados network`` …) but are kept off this list
so the common path stays uncluttered.
"""

from __future__ import annotations

import click

from ados.cli import _ansi

# Command, one-line description. Order is the order shown.
_PRIMARY: list[tuple[str, str]] = [
    ("ados", "live status dashboard"),
    ("ados status", "status at a glance (--json for scripts)"),
    ("ados pair", "connect this agent to Mission Control"),
    ("ados unpair", "release this agent"),
    ("ados update", "update the agent"),
    ("ados uninstall", "remove the agent"),
    ("ados logs", "view agent logs"),
    ("ados help", "show this overview"),
]

_ADVANCED = ("rust", "plugin", "network", "radio", "hardware", "profile")


def render_help(theme: _ansi.Theme | None = None) -> list[str]:
    """The curated help as a list of (styled) lines."""
    theme = theme or _ansi.detect_theme()
    sep = " / " if theme.ascii else " · "
    width = max(len(cmd) for cmd, _ in _PRIMARY)
    lines = [_ansi.marker(theme, "ADOS drone agent"), ""]
    for cmd, desc in _PRIMARY:
        lines.append(f"  {theme.accent(cmd.ljust(width))}   {theme.dim(desc)}")
    lines.append("")
    lines.append(f"  {theme.dim('Advanced:')} {sep.join(_ADVANCED)}")
    hint = "          run 'ados <group> --help' for these"
    lines.append(f"  {theme.dim(hint)}")
    return lines


@click.command(name="help")
def help_command() -> None:
    """Show the ADOS command overview."""
    theme = _ansi.detect_theme()
    for line in render_help(theme):
        click.echo(line)
