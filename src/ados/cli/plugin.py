"""``ados plugin`` CLI subcommand tree.

Wires the plugin supervisor to the operator's terminal. Mirrors the
spec at ``product/specs/ados-plugin-system/12-cli.md``. Output is
human-readable by default; ``--json`` switches to a machine envelope
``{"ok": bool, "code": int, "kind": str, "data": ...}`` per the spec.

Exit code map (matches spec §5):

* 0 success
* 1 generic failure
* 2 manifest invalid
* 3 signature invalid
* 4 permission denied (capability change)
* 5 plugin not found
* 6 wrong state
* 7 resource limit
* 8 compatibility failed
"""

from __future__ import annotations

import json
import sys
from dataclasses import asdict
from pathlib import Path

import click

from ados.plugins.errors import (
    ArchiveError,
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.supervisor import PluginSupervisor

EXIT_OK = 0
EXIT_GENERIC = 1
EXIT_MANIFEST_INVALID = 2
EXIT_SIGNATURE_INVALID = 3
EXIT_PERMISSION_DENIED = 4
EXIT_NOT_FOUND = 5
EXIT_WRONG_STATE = 6
EXIT_RESOURCE_LIMIT = 7
EXIT_COMPATIBILITY = 8

KIND_BY_CODE = {
    EXIT_OK: "ok",
    EXIT_GENERIC: "generic_failure",
    EXIT_MANIFEST_INVALID: "manifest_invalid",
    EXIT_SIGNATURE_INVALID: "signature_invalid",
    EXIT_PERMISSION_DENIED: "permission_denied",
    EXIT_NOT_FOUND: "plugin_not_found",
    EXIT_WRONG_STATE: "wrong_state",
    EXIT_RESOURCE_LIMIT: "resource_limit",
    EXIT_COMPATIBILITY: "compatibility_failed",
}


def _emit_ok(as_json: bool, data: dict | list | None = None) -> None:
    if as_json:
        click.echo(json.dumps({"ok": True, "code": 0, "kind": "ok", "data": data}))


def _emit_err(
    as_json: bool, code: int, message: str, hint: str | None = None
) -> None:
    if as_json:
        envelope = {
            "ok": False,
            "code": code,
            "kind": KIND_BY_CODE.get(code, "generic_failure"),
            "detail": message,
        }
        if hint:
            envelope["hint"] = hint
        click.echo(json.dumps(envelope))
    else:
        click.echo(f"Error: {message}", err=True)
        if hint:
            click.echo(f"Hint: {hint}", err=True)


def _make_supervisor(*, allow_unsigned: bool = False) -> PluginSupervisor:
    sup = PluginSupervisor(require_signed=not allow_unsigned)
    sup.discover()
    return sup


@click.group("plugin", help="Install, enable, and inspect plugins.")
def plugin_group() -> None:
    pass


@plugin_group.command("list", help="List installed plugins.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
@click.option(
    "--all",
    "show_all",
    is_flag=True,
    help="Include built-in plugins discovered via entry-points.",
)
def list_plugins(as_json: bool, show_all: bool) -> None:
    sup = _make_supervisor()
    rows = []
    for inst in sup.installs():
        rows.append(
            {
                "id": inst.plugin_id,
                "version": inst.version,
                "status": inst.status,
                "signer": inst.signer_id,
                "kind": "third-party",
            }
        )
    if show_all:
        for plugin_id, manifest in sup.builtin_manifests().items():
            rows.append(
                {
                    "id": plugin_id,
                    "version": manifest.version,
                    "status": "builtin",
                    "signer": "altnautica",
                    "kind": "built-in",
                }
            )
    if as_json:
        _emit_ok(as_json, rows)
        return
    if not rows:
        click.echo("No plugins installed.")
        return
    click.echo(f"{'ID':40} {'VERSION':10} {'STATUS':12} SIGNER")
    for row in rows:
        click.echo(
            f"{row['id']:40} {row['version']:10} {row['status']:12} "
            f"{row['signer'] or '-'}"
        )


@plugin_group.command("install", help="Install a .adosplug archive.")
@click.argument("archive", type=click.Path(exists=True, dir_okay=False))
@click.option(
    "--allow-unsigned",
    is_flag=True,
    help="Skip signature verification (developer mode only).",
)
@click.option(
    "--yes",
    "auto_yes",
    is_flag=True,
    help="Skip permission approval prompt; refuses high/critical permissions.",
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def install(
    archive: str, allow_unsigned: bool, auto_yes: bool, as_json: bool
) -> None:
    try:
        sup = _make_supervisor(allow_unsigned=allow_unsigned)
        result = sup.install_archive(Path(archive))
    except ManifestError as exc:
        _emit_err(as_json, EXIT_MANIFEST_INVALID, str(exc))
        sys.exit(EXIT_MANIFEST_INVALID)
    except SignatureError as exc:
        _emit_err(
            as_json,
            EXIT_SIGNATURE_INVALID,
            str(exc),
            hint="Run with --allow-unsigned in developer mode.",
        )
        sys.exit(EXIT_SIGNATURE_INVALID)
    except ArchiveError as exc:
        _emit_err(as_json, EXIT_GENERIC, str(exc))
        sys.exit(EXIT_GENERIC)
    except SupervisorError as exc:
        _emit_err(as_json, EXIT_COMPATIBILITY, str(exc))
        sys.exit(EXIT_COMPATIBILITY)

    # Permission approval flow (textual; the GCS uses a richer dialog).
    if not as_json:
        click.echo(f"Plugin: {result.plugin_id} v{result.version}")
        click.echo(f"Risk:   {result.risk}")
        click.echo("Permissions requested:")
        for perm in result.permissions_requested:
            click.echo(f"  - {perm}")
    if not auto_yes and not as_json:
        approved = click.confirm("Approve permissions?", default=False)
        if not approved:
            sup.remove(result.plugin_id, keep_data=False)
            click.echo("Install cancelled. Plugin uninstalled.")
            sys.exit(EXIT_OK)
        for perm in result.permissions_requested:
            sup.grant_permission(result.plugin_id, perm)
    elif auto_yes:
        if result.risk in ("high", "critical"):
            _emit_err(
                as_json,
                EXIT_PERMISSION_DENIED,
                f"--yes refuses {result.risk}-risk plugins",
                hint="Re-run interactively or pass --accept-risk.",
            )
            sup.remove(result.plugin_id, keep_data=False)
            sys.exit(EXIT_PERMISSION_DENIED)
        for perm in result.permissions_requested:
            sup.grant_permission(result.plugin_id, perm)

    if as_json:
        _emit_ok(as_json, asdict(result))
    else:
        click.echo(f"Installed {result.plugin_id} v{result.version}.")
        click.echo(f"Run: ados plugin enable {result.plugin_id}")


@plugin_group.command("enable", help="Enable an installed plugin.")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def enable(plugin_id: str, as_json: bool) -> None:
    try:
        sup = _make_supervisor()
        sup.enable(plugin_id)
    except SupervisorError as exc:
        _emit_err(as_json, EXIT_NOT_FOUND, str(exc))
        sys.exit(EXIT_NOT_FOUND)
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "running"})
    if not as_json:
        click.echo(f"{plugin_id}: enabled.")


@plugin_group.command("disable", help="Disable a plugin (keeps it installed).")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def disable(plugin_id: str, as_json: bool) -> None:
    try:
        sup = _make_supervisor()
        sup.disable(plugin_id)
    except SupervisorError as exc:
        _emit_err(as_json, EXIT_NOT_FOUND, str(exc))
        sys.exit(EXIT_NOT_FOUND)
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "disabled"})
    if not as_json:
        click.echo(f"{plugin_id}: disabled.")


@plugin_group.command("remove", help="Stop, uninstall, and forget a plugin.")
@click.argument("plugin_id")
@click.option("--keep-data", is_flag=True, help="Preserve plugin data directory.")
@click.option("--json", "as_json", is_flag=True)
def remove(plugin_id: str, keep_data: bool, as_json: bool) -> None:
    try:
        sup = _make_supervisor()
        sup.remove(plugin_id, keep_data=keep_data)
    except SupervisorError as exc:
        _emit_err(as_json, EXIT_NOT_FOUND, str(exc))
        sys.exit(EXIT_NOT_FOUND)
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "removed"})
    if not as_json:
        click.echo(f"{plugin_id}: removed.")


@plugin_group.command(
    "perms", help="Show or revoke permissions on an installed plugin."
)
@click.argument("plugin_id")
@click.option(
    "--revoke",
    "revoke_id",
    default=None,
    help="Revoke a specific permission id.",
)
@click.option(
    "--yes",
    "-y",
    "auto_yes",
    is_flag=True,
    help="Skip the confirmation prompt when revoking a permission.",
)
@click.option("--json", "as_json", is_flag=True)
def perms(
    plugin_id: str, revoke_id: str | None, auto_yes: bool, as_json: bool
) -> None:
    sup = _make_supervisor()
    install = next(
        (i for i in sup.installs() if i.plugin_id == plugin_id), None
    )
    if install is None:
        _emit_err(as_json, EXIT_NOT_FOUND, f"plugin {plugin_id} not installed")
        sys.exit(EXIT_NOT_FOUND)
    if revoke_id:
        # Confirm intent before revoking a granted capability. The
        # plugin loses access to the protected resource on the next
        # token rotation, which can break a running workload. JSON
        # callers and `--yes` operators skip the prompt.
        if not auto_yes and not as_json:
            click.echo(
                f"About to revoke '{revoke_id}' from '{plugin_id}'."
            )
            click.echo(
                "The plugin will lose access to the protected resource immediately."
            )
            if not click.confirm("Continue?", default=False):
                click.echo("Revoke cancelled.")
                sys.exit(EXIT_OK)
        try:
            from ados.plugins.state import revoke_permission, save_state, state_lock

            with state_lock():
                revoke_permission(install, revoke_id)
                save_state(sup.installs())
        except Exception as exc:  # noqa: BLE001
            _emit_err(as_json, EXIT_GENERIC, str(exc))
            sys.exit(EXIT_GENERIC)
        _emit_ok(as_json, {"plugin_id": plugin_id, "revoked": revoke_id})
        if not as_json:
            click.echo(f"{plugin_id}: revoked {revoke_id}.")
        return
    rows = [
        {
            "permission_id": pid,
            "granted": grant.granted,
            "granted_at": grant.granted_at,
            "revoked_at": grant.revoked_at,
        }
        for pid, grant in sorted(install.permissions.items())
    ]
    if as_json:
        _emit_ok(as_json, rows)
        return
    if not rows:
        click.echo(f"{plugin_id}: no permissions recorded.")
        return
    click.echo(f"{'PERMISSION':30} STATE")
    for row in rows:
        state = "GRANTED" if row["granted"] else "DENIED"
        click.echo(f"{row['permission_id']:30} {state}")


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
@click.option("--json", "as_json", is_flag=True)
def logs(plugin_id: str, lines: int, follow: bool, as_json: bool) -> None:
    from ados.core.paths import PLUGIN_LOG_DIR

    log_path = PLUGIN_LOG_DIR / f"{plugin_id.replace('.', '-')}.log"
    if not log_path.exists():
        _emit_err(
            as_json,
            EXIT_NOT_FOUND,
            f"no log file at {log_path}",
            hint="Plugin may never have started, or logs rotated out.",
        )
        sys.exit(EXIT_NOT_FOUND)
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


@plugin_group.command("info", help="Print manifest summary and runtime state.")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def info(plugin_id: str, as_json: bool) -> None:
    sup = _make_supervisor()
    install = next(
        (i for i in sup.installs() if i.plugin_id == plugin_id), None
    )
    builtin = sup.builtin_manifests().get(plugin_id)
    if install is None and builtin is None:
        _emit_err(
            as_json, EXIT_NOT_FOUND, f"plugin {plugin_id} is not installed"
        )
        sys.exit(EXIT_NOT_FOUND)
    payload = {
        "plugin_id": plugin_id,
        "install": asdict(install)
        if install is not None
        else None,
        "is_builtin": builtin is not None,
    }
    if as_json:
        _emit_ok(as_json, payload)
        return
    if install:
        click.echo(f"Plugin:   {install.plugin_id}")
        click.echo(f"Version:  {install.version}")
        click.echo(f"Status:   {install.status}")
        click.echo(f"Signer:   {install.signer_id or '-'}")
        click.echo(f"Source:   {install.source}")
        click.echo("Permissions:")
        for pid, grant in sorted(install.permissions.items()):
            state = "GRANTED" if grant.granted else "DENIED"
            click.echo(f"  {pid:30} {state}")
    else:
        click.echo(f"Built-in plugin: {plugin_id}")
        click.echo(f"Version: {builtin.version}")


@plugin_group.command(
    "lint", help="Run static analysis on a .adosplug archive before submission."
)
@click.argument("archive_path", type=click.Path(exists=True, dir_okay=False))
@click.option("--json", "as_json", is_flag=True)
def lint(archive_path: str, as_json: bool) -> None:
    from ados.plugins.lint import format_report, lint_archive

    try:
        report = lint_archive(archive_path)
    except (ArchiveError, ManifestError) as exc:
        code = (
            EXIT_MANIFEST_INVALID
            if isinstance(exc, ManifestError)
            else EXIT_GENERIC
        )
        _emit_err(as_json, code, str(exc))
        sys.exit(code)

    if as_json:
        _emit_ok(as_json, report.to_dict())
    else:
        click.echo(format_report(report))

    sys.exit(EXIT_OK if report.passed else EXIT_GENERIC)


@plugin_group.command(
    "test",
    help="Run a plugin's pytest suite under the SDK test harness.",
)
@click.argument(
    "plugin_dir",
    type=click.Path(exists=True, file_okay=False, dir_okay=True),
)
@click.option(
    "--tests-dir",
    "tests_dir",
    default="tests",
    show_default=True,
    help="Subdirectory under plugin_dir containing pytest tests.",
)
@click.option(
    "-k",
    "expression",
    default=None,
    help="pytest -k expression passed through to the runner.",
)
@click.option("--json", "as_json", is_flag=True)
def test_plugin(
    plugin_dir: str,
    tests_dir: str,
    expression: str | None,
    as_json: bool,
) -> None:
    """Drive pytest against the plugin's tests with the harness available
    via a conftest fixture the plugin author registers themselves. The
    subcommand validates the plugin manifest, then shells out to
    ``pytest`` so the runner stays the canonical one author tests
    were written against.
    """
    from ados.plugins.manifest import PluginManifest

    plugin_root = Path(plugin_dir)
    manifest_path = plugin_root / "manifest.yaml"
    if not manifest_path.is_file():
        _emit_err(
            as_json,
            EXIT_MANIFEST_INVALID,
            f"manifest.yaml not found at {manifest_path}",
        )
        sys.exit(EXIT_MANIFEST_INVALID)

    try:
        manifest = PluginManifest.from_yaml_file(manifest_path)
    except ManifestError as exc:
        _emit_err(as_json, EXIT_MANIFEST_INVALID, str(exc))
        sys.exit(EXIT_MANIFEST_INVALID)

    tests_path = plugin_root / tests_dir
    if not tests_path.is_dir():
        _emit_err(
            as_json,
            EXIT_NOT_FOUND,
            f"tests directory {tests_path} not found",
        )
        sys.exit(EXIT_NOT_FOUND)

    import os
    import subprocess

    env = os.environ.copy()
    env["ADOS_PLUGIN_ID"] = manifest.id
    env["ADOS_PLUGIN_VERSION"] = manifest.version
    env["ADOS_PLUGIN_ROOT"] = str(plugin_root.resolve())
    if manifest.agent and manifest.agent.test_fixtures:
        env["ADOS_PLUGIN_TEST_FIXTURES"] = json.dumps(
            manifest.agent.test_fixtures
        )

    cmd = [sys.executable, "-m", "pytest", str(tests_path)]
    if expression:
        cmd.extend(["-k", expression])
    rc = subprocess.call(cmd, env=env)
    if rc == 0:
        _emit_ok(as_json, {"plugin_id": manifest.id})
        sys.exit(EXIT_OK)
    _emit_err(as_json, EXIT_GENERIC, f"pytest exited with {rc}")
    sys.exit(EXIT_GENERIC)
