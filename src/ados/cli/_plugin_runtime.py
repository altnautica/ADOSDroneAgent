"""``ados plugin`` runtime: logs, pin, unpin, auto-update, info."""

from __future__ import annotations

import os
import sys
from pathlib import Path

import click

from ados.cli._plugin_common import (
    EXIT_GENERIC,
    EXIT_MANIFEST_INVALID,
    EXIT_NOT_FOUND,
    EXIT_PERMISSION_DENIED,
    _agent,
    _call,
    _emit_err,
    _emit_ok,
    _fail,
    _failure,
    _seg,
    plugin_group,
)
from ados.plugins.builtin import builtin_manifest


@plugin_group.command("logs", help="Tail a plugin's stdout/stderr log file.")
@click.argument("plugin_id")
@click.option(
    "--lines", "lines", type=int, default=100, help="Number of lines to print."
)
@click.option(
    "--follow",
    is_flag=True,
    help="Follow the log (like tail -f). Ctrl-C to stop.",
)
@click.option(
    "--service",
    "service",
    default=None,
    help="Read a declared service's log instead of the main runner's.",
)
@click.option("--json", "as_json", is_flag=True)
def logs(
    plugin_id: str, lines: int, follow: bool, service: str | None, as_json: bool
) -> None:
    # The host owns the unit and log naming; ask it rather than re-deriving.
    detail = _agent("GET", f"/api/plugins/{_seg(plugin_id)}", as_json)
    detail = detail if isinstance(detail, dict) else {}
    if service is not None:
        service_logs = detail.get("service_logs") or {}
        if service not in service_logs:
            known = ", ".join(sorted(service_logs)) or "none"
            _fail(
                as_json,
                EXIT_MANIFEST_INVALID,
                f"{plugin_id} declares no service {service!r}",
                hint=f"Declared services: {known}.",
            )
        raw_path = service_logs[service]
    else:
        raw_path = detail.get("log_path")
    if not raw_path:
        _fail(as_json, EXIT_GENERIC, f"agent reported no log path for {plugin_id}")
    log_path = Path(raw_path)
    try:
        present = log_path.exists()
    except PermissionError:
        # The log directory is closed to this user; the access check below says so.
        present = True
    if not present:
        _fail(
            as_json,
            EXIT_NOT_FOUND,
            f"no log file at {log_path}",
            hint="Plugin may never have started, or logs rotated out.",
        )
    if not os.access(log_path, os.R_OK):
        _fail(
            as_json,
            EXIT_PERMISSION_DENIED,
            f"cannot read {log_path}: plugin logs are readable by root only",
            hint=f"Run it as root: sudo ados plugin logs {plugin_id}",
        )
    if follow:
        # Delegate to system tail -f for the follow case so we don't
        # reinvent line buffering.
        import subprocess

        try:
            subprocess.run(
                ["tail", "-n", str(lines), "-f", str(log_path)], check=False
            )
        except KeyboardInterrupt:
            pass
        return
    # Non-follow path: read last N lines.
    try:
        with open(log_path, encoding="utf-8", errors="replace") as fh:
            tail = fh.readlines()[-lines:]
    except OSError as exc:
        _emit_err(as_json, EXIT_GENERIC, str(exc))
        sys.exit(EXIT_GENERIC)
    if as_json:
        _emit_ok(as_json, {"plugin_id": plugin_id, "lines": tail})
        return
    for line in tail:
        click.echo(line.rstrip())



@plugin_group.command(
    "pin",
    help="Pin a plugin to a version; auto-update will skip it.",
)
@click.argument("plugin_id")
@click.argument("version")
@click.option("--json", "as_json", is_flag=True)
def pin(plugin_id: str, version: str, as_json: bool) -> None:
    _agent(
        "POST",
        f"/api/plugins/{_seg(plugin_id)}/pin",
        as_json,
        json_body={"version": version},
    )
    _emit_ok(as_json, {"plugin_id": plugin_id, "pinned_version": version})
    if not as_json:
        click.echo(f"{plugin_id}: pinned to {version}.")


@plugin_group.command(
    "unpin",
    help="Clear the pinned version on a plugin so auto-update can run.",
)
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def unpin(plugin_id: str, as_json: bool) -> None:
    _agent("POST", f"/api/plugins/{_seg(plugin_id)}/unpin", as_json)
    _emit_ok(as_json, {"plugin_id": plugin_id, "pinned_version": None})
    if not as_json:
        click.echo(f"{plugin_id}: unpinned.")


@plugin_group.command(
    "auto-update",
    help="Toggle auto-update on or off for a plugin.",
)
@click.argument("plugin_id")
@click.argument("state", type=click.Choice(["on", "off"]))
@click.option("--json", "as_json", is_flag=True)
def auto_update(plugin_id: str, state: str, as_json: bool) -> None:
    enabled = state == "on"
    _agent(
        "POST",
        f"/api/plugins/{_seg(plugin_id)}/auto-update",
        as_json,
        json_body={"enabled": enabled},
    )
    _emit_ok(as_json, {"plugin_id": plugin_id, "auto_update": enabled})
    if not as_json:
        click.echo(f"{plugin_id}: auto-update {state}.")


@plugin_group.command("info", help="Print manifest summary and runtime state.")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def info(plugin_id: str, as_json: bool) -> None:
    response = _call("GET", f"/api/plugins/{_seg(plugin_id)}", as_json)
    builtin = builtin_manifest(plugin_id)
    if not response.ok:
        exit_code, message = _failure(response)
        if exit_code != EXIT_NOT_FOUND or builtin is None:
            _fail(as_json, exit_code, message)
    detail = response.body if response.ok else None
    install = detail["install"] if detail is not None else None
    payload = {
        "plugin_id": plugin_id,
        "install": install,
        "manifest": detail["manifest"] if detail is not None else None,
        "granted_capabilities": (
            detail.get("granted_capabilities", []) if detail is not None else []
        ),
        "is_builtin": builtin is not None,
        "available_version": builtin.version if builtin is not None else None,
    }
    if as_json:
        _emit_ok(as_json, payload)
        return
    if install is not None:
        click.echo(f"Plugin:   {install['plugin_id']}")
        click.echo(f"Version:  {install['version']}")
        click.echo(f"Status:   {install['status']}")
        click.echo(f"Signer:   {install.get('signer_id') or '-'}")
        click.echo(f"Source:   {install.get('source')}")
        click.echo("Permissions:")
        for pid, grant in sorted(install.get("permissions", {}).items()):
            state = "GRANTED" if grant.get("granted") else "DENIED"
            click.echo(f"  {pid:30} {state}")
    elif builtin is not None:
        click.echo(f"Built-in plugin: {plugin_id} (not installed)")
        click.echo(f"Version: {builtin.version}")
        click.echo(f"Install: ados plugin install {plugin_id}")
