"""``ados plugin`` CLI subcommand tree.

The lifecycle commands (``list``, ``info``, ``install``, ``enable``,
``disable``, ``remove``, ``perms``, ``pin``, ``unpin``, ``auto-update``)
drive the native plugin lifecycle over the local control surface
(``/api/plugins``); the agent owns plugin state, units and signature checks.
``logs`` reads the plugin log files, and ``lint``, ``test``, ``sign`` and
``keygen`` are local developer tools.

Output is human-readable by default; ``--json`` switches to a machine
envelope ``{"ok": bool, "code": int, "kind": str, "data": ...}``.

Exit code map:

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
from pathlib import Path
from typing import Any, NoReturn
from urllib.parse import quote

import click

from ados.cli import _agent_http
from ados.plugins.builtin import builtin_manifest, builtin_manifests
from ados.plugins.errors import ArchiveError, ManifestError

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

# The agent's lifecycle error envelope ``{ok: false, code, kind, detail}``
# numbers its codes; this maps them onto the CLI exit codes above.
_EXIT_BY_AGENT_CODE = {
    10: EXIT_SIGNATURE_INVALID,
    11: EXIT_PERMISSION_DENIED,
    12: EXIT_MANIFEST_INVALID,
    13: EXIT_RESOURCE_LIMIT,
    14: EXIT_NOT_FOUND,
    17: EXIT_COMPATIBILITY,
    18: EXIT_COMPATIBILITY,  # incompatible: target profile, host arch, ...
    19: EXIT_PERMISSION_DENIED,  # refused: first-party-only gates
}

_UNSIGNED_HINT = (
    "The agent installs only signed archives unless its services run with "
    "ADOS_PLUGIN_REQUIRE_SIGNED=0 (developer mode)."
)


def _emit_ok(as_json: bool, data: dict[str, Any] | list[Any] | None = None) -> None:
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


def _fail(as_json: bool, code: int, message: str, hint: str | None = None) -> NoReturn:
    _emit_err(as_json, code, message, hint)
    sys.exit(code)


def _seg(value: str) -> str:
    """One URL path segment, escaped so an argument can never add another."""
    return quote(value, safe="")


def _call(method: str, path: str, as_json: bool, **kwargs: Any) -> _agent_http.AgentResponse:
    """One control-surface request; an unreachable or refusing agent exits here."""
    try:
        return _agent_http.call(method, path, **kwargs)
    except click.ClickException as exc:
        _fail(as_json, EXIT_GENERIC, exc.message)


def _failure(response: _agent_http.AgentResponse) -> tuple[int, str]:
    """The CLI exit code and message for an agent error answer."""
    body = response.body
    if isinstance(body, dict):
        code = body.get("code")
        message = str(body.get("detail") or f"agent answered HTTP {response.status}")
    else:
        # Not the lifecycle envelope (a proxy page, an empty 404): name the
        # status and keep only the start of the text.
        code = None
        text = str(body).strip()
        message = f"agent answered HTTP {response.status}" + (f": {text[:200]}" if text else "")
    if isinstance(code, int) and code in _EXIT_BY_AGENT_CODE:
        return _EXIT_BY_AGENT_CODE[code], message
    if message.startswith("incompatible:"):
        return EXIT_COMPATIBILITY, message
    if response.status == 404:
        return EXIT_NOT_FOUND, message
    return EXIT_GENERIC, message


def _agent(method: str, path: str, as_json: bool, **kwargs: Any) -> Any:
    """Call the control surface and return the body of a 2xx answer.

    An error answer is printed through the CLI envelope and exits with the
    mapped code.
    """
    response = _call(method, path, as_json, **kwargs)
    if not response.ok:
        exit_code, message = _failure(response)
        hint = _UNSIGNED_HINT if exit_code == EXIT_SIGNATURE_INVALID else None
        _fail(as_json, exit_code, message, hint)
    return response.body


def _is_builtin_install(install: dict[str, Any]) -> bool:
    return str(install.get("source_uri") or "").startswith("builtin:")


@click.group("plugin", help="Install, enable, and inspect plugins.")
def plugin_group() -> None:
    pass


@plugin_group.command("list", help="List installed plugins.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
@click.option(
    "--all",
    "show_all",
    is_flag=True,
    help="Include built-in plugins that ship with the agent and are not installed.",
)
def list_plugins(as_json: bool, show_all: bool) -> None:
    body = _agent("GET", "/api/plugins", as_json)
    rows = [
        {
            "id": inst["plugin_id"],
            "version": inst["version"],
            "status": inst["status"],
            "signer": inst.get("signer_id"),
            "kind": "built-in" if _is_builtin_install(inst) else "third-party",
        }
        for inst in body.get("installs", [])
    ]
    if show_all:
        installed = {row["id"] for row in rows}
        for plugin_id, manifest in builtin_manifests().items():
            if plugin_id in installed:
                continue
            rows.append(
                {
                    "id": plugin_id,
                    "version": manifest.version,
                    "status": "available",
                    "signer": None,
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


def _catalog_entry(plugin_id: str, as_json: bool) -> dict[str, Any] | None:
    catalog = _agent("GET", "/api/v1/plugins/catalog", as_json)
    plugins = catalog.get("plugins", []) if isinstance(catalog, dict) else []
    return next((p for p in plugins if p.get("id") == plugin_id), None)


def _plan_install(
    source: str, as_json: bool
) -> tuple[dict[str, Any], str, dict[str, Any]]:
    """Resolve ``source`` to a consent preview plus the install request to send.

    Returns ``(summary, install_path, request_kwargs)``. The summary carries
    ``plugin_id``, ``version``, ``risk`` and ``permissions`` for the approval
    prompt; nothing is installed yet.
    """
    archive = Path(source)
    if archive.is_file():
        upload = {"file": (archive.name, archive.read_bytes(), "application/zip")}
        summary = _agent("POST", "/api/plugins/parse", as_json, files=upload)
        return summary, "/api/plugins/install", {"files": upload}
    builtin = builtin_manifest(source)
    if builtin is not None:
        summary = {
            "plugin_id": builtin.id,
            "version": builtin.version,
            "risk": builtin.risk,
            "permissions": [{"id": p} for p in sorted(builtin.declared_permissions())],
        }
        return summary, "/api/plugins/install_builtin", {"json_body": {"plugin_id": source}}
    if source.startswith(("https://", "http://")):
        summary = _agent("POST", "/api/plugins/parse_from_url", as_json, json_body={"url": source})
        # Pin the install to the exact bytes the operator just reviewed.
        body = {"url": source, "expected_sha256": summary.get("archive_sha256")}
        return summary, "/api/plugins/install_from_url", {"json_body": body}
    entry = _catalog_entry(source, as_json)
    if entry is None:
        _fail(
            as_json,
            EXIT_NOT_FOUND,
            f"{source} is not an archive file, a built-in plugin id, a URL, "
            "or a catalog plugin id",
            hint="List built-in plugins with: ados plugin list --all",
        )
    url = entry.get("download_url") or ""
    sha256 = entry.get("archive_sha256") or ""
    if not url or not sha256:
        _fail(as_json, EXIT_NOT_FOUND, f"catalog entry {source} has no published download yet")
    summary = _agent(
        "POST",
        "/api/plugins/parse_from_url",
        as_json,
        json_body={"url": url, "expected_sha256": sha256},
    )
    body = {"url": url, "expected_sha256": sha256, "from_catalog": True}
    return summary, "/api/plugins/install_from_url", {"json_body": body}


def _grant_all(plugin_id: str, permissions: list[str], as_json: bool) -> list[str]:
    """Grant each permission; one the agent refuses is reported and skipped."""
    granted: list[str] = []
    for perm in permissions:
        response = _call(
            "POST",
            f"/api/plugins/{_seg(plugin_id)}/grant",
            as_json,
            json_body={"permission_id": perm},
        )
        if response.ok:
            granted.append(perm)
        else:
            click.echo(f"Warning: {perm} not granted: {_failure(response)[1]}", err=True)
    return granted


@plugin_group.command(
    "install",
    help=(
        "Install a .adosplug archive, a built-in plugin by id, an archive URL, "
        "or a first-party catalog plugin by id."
    ),
)
@click.argument("source")
@click.option(
    "--yes",
    "auto_yes",
    is_flag=True,
    help="Skip permission approval prompt; refuses high/critical permissions.",
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def install(source: str, auto_yes: bool, as_json: bool) -> None:
    summary, install_path, request = _plan_install(source, as_json)
    risk = summary.get("risk")
    if not as_json:
        click.echo(f"Plugin: {summary.get('plugin_id')} v{summary.get('version')}")
        click.echo(f"Risk:   {risk}")
        click.echo("Permissions requested:")
        for perm in summary.get("permissions", []):
            label = perm.get("label")
            click.echo(f"  - {perm['id']}" + (f" ({label})" if label else ""))

    # Permission approval (textual; the GCS uses a richer dialog). Consent is
    # given before anything is installed, so a declined install leaves nothing.
    grant = False
    if auto_yes:
        if risk in ("high", "critical"):
            _fail(
                as_json,
                EXIT_PERMISSION_DENIED,
                f"--yes refuses {risk}-risk plugins",
                hint="Re-run without --yes and approve the permissions after review.",
            )
        grant = True
    elif not as_json:
        if not click.confirm("Approve permissions?", default=False):
            click.echo("Install cancelled.")
            sys.exit(EXIT_OK)
        grant = True

    result = _agent("POST", install_path, as_json, timeout=None, **request)
    plugin_id = result["plugin_id"]
    requested = list(result.get("permissions_requested") or [])
    granted = _grant_all(plugin_id, requested, as_json) if grant else []
    data = {
        "plugin_id": plugin_id,
        "version": result.get("version"),
        "signer_id": result.get("signer_id"),
        "risk": result.get("risk"),
        "permissions_requested": requested,
        "granted": granted,
    }
    if as_json:
        _emit_ok(as_json, data)
    else:
        click.echo(f"Installed {plugin_id} v{result.get('version')}.")
        click.echo(f"Run: ados plugin enable {plugin_id}")


@plugin_group.command("enable", help="Enable an installed plugin.")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def enable(plugin_id: str, as_json: bool) -> None:
    _agent("POST", f"/api/plugins/{_seg(plugin_id)}/enable", as_json, timeout=None)
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "running"})
    if not as_json:
        click.echo(f"{plugin_id}: enabled.")


@plugin_group.command("disable", help="Disable a plugin (keeps it installed).")
@click.argument("plugin_id")
@click.option("--json", "as_json", is_flag=True)
def disable(plugin_id: str, as_json: bool) -> None:
    _agent("POST", f"/api/plugins/{_seg(plugin_id)}/disable", as_json)
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "disabled"})
    if not as_json:
        click.echo(f"{plugin_id}: disabled.")


@plugin_group.command("remove", help="Stop, uninstall, and forget a plugin.")
@click.argument("plugin_id")
@click.option("--keep-data", is_flag=True, help="Preserve plugin data directory.")
@click.option("--json", "as_json", is_flag=True)
def remove(plugin_id: str, keep_data: bool, as_json: bool) -> None:
    _agent(
        "DELETE",
        f"/api/plugins/{_seg(plugin_id)}",
        as_json,
        params={"keep_data": "true" if keep_data else "false"},
    )
    _emit_ok(as_json, {"plugin_id": plugin_id, "status": "removed"})
    if not as_json:
        click.echo(f"{plugin_id}: removed.")


@plugin_group.command(
    "perms", help="Show, grant, or revoke permissions on an installed plugin."
)
@click.argument("plugin_id")
@click.option(
    "--grant",
    "grant_id",
    default=None,
    help="Grant a permission the plugin declares.",
)
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
    plugin_id: str,
    grant_id: str | None,
    revoke_id: str | None,
    auto_yes: bool,
    as_json: bool,
) -> None:
    if grant_id and revoke_id:
        _fail(as_json, EXIT_GENERIC, "pass either --grant or --revoke, not both")
    if grant_id:
        _agent(
            "POST",
            f"/api/plugins/{_seg(plugin_id)}/grant",
            as_json,
            json_body={"permission_id": grant_id},
        )
        _emit_ok(as_json, {"plugin_id": plugin_id, "granted": grant_id})
        if not as_json:
            click.echo(f"{plugin_id}: granted {grant_id}.")
        return
    if revoke_id:
        # Confirm intent before revoking a granted capability. The plugin
        # loses access to the protected resource, which can break a running
        # workload. JSON callers and `--yes` operators skip the prompt.
        if not auto_yes and not as_json:
            click.echo(f"About to revoke '{revoke_id}' from '{plugin_id}'.")
            click.echo(
                "The plugin will lose access to the protected resource immediately."
            )
            if not click.confirm("Continue?", default=False):
                click.echo("Revoke cancelled.")
                sys.exit(EXIT_OK)
        result = _agent(
            "DELETE",
            f"/api/plugins/{_seg(plugin_id)}/perms/{_seg(revoke_id)}",
            as_json,
        )
        _emit_ok(
            as_json,
            {
                "plugin_id": plugin_id,
                "revoked": revoke_id,
                "granted": result.get("granted", []),
            },
        )
        if not as_json:
            click.echo(f"{plugin_id}: revoked {revoke_id}.")
        return
    detail = _agent("GET", f"/api/plugins/{_seg(plugin_id)}", as_json)
    rows = [
        {
            "permission_id": pid,
            "granted": grant.get("granted", False),
            "granted_at": grant.get("granted_at"),
        }
        for pid, grant in sorted(detail["install"].get("permissions", {}).items())
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


@plugin_group.command(
    "sign",
    help=(
        "Pack a plugin directory into a signed .adosplug archive. "
        "Signs the canonical payload hash with an Ed25519 private key "
        "(PEM-encoded) and writes the SIGNATURE file inside the archive."
    ),
)
@click.argument(
    "plugin_dir",
    type=click.Path(exists=True, file_okay=False, dir_okay=True),
)
@click.option(
    "--key",
    "key_path",
    required=True,
    type=click.Path(exists=True, dir_okay=False),
    help="Path to the Ed25519 private key in PEM format.",
)
@click.option(
    "--signer-id",
    "signer_id",
    required=True,
    help="Signer identifier written to the SIGNATURE file. "
    "Must match the public-key filename on the agent (signer-id.pem).",
)
@click.option(
    "--output",
    "output_path",
    required=True,
    type=click.Path(dir_okay=False),
    help="Output path for the signed .adosplug archive.",
)
@click.option("--json", "as_json", is_flag=True)
def sign_plugin(
    plugin_dir: str,
    key_path: str,
    signer_id: str,
    output_path: str,
    as_json: bool,
) -> None:
    """Sign a plugin directory and emit a ready-to-distribute archive.

    This is a developer-side command. It is intentionally a thin wrapper
    around the canonical archive packer and the well-known canonical
    payload hash, so the resulting archive matches what
    :func:`ados.plugins.archive.parse_archive_bytes` and the agent's
    signature verifier expect.
    """
    import base64
    import hashlib
    import io
    import zipfile

    from ados.plugins.archive import (
        MANIFEST_FILENAME,
        SIGNATURE_FILENAME,
        _canonical_payload_hash,
        pack_directory,
    )
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

    output = Path(output_path)
    output.parent.mkdir(parents=True, exist_ok=True)

    # Pack the unsigned archive to a temp path first; the signing step
    # re-writes it with the SIGNATURE entry appended.
    import tempfile

    tmp_handle, tmp_name = tempfile.mkstemp(
        suffix=".adosplug", dir=str(output.parent)
    )
    import os

    os.close(tmp_handle)
    tmp_archive = Path(tmp_name)
    try:
        try:
            pack_directory(plugin_root, manifest, tmp_archive)
        except ArchiveError as exc:
            _emit_err(as_json, EXIT_GENERIC, str(exc))
            sys.exit(EXIT_GENERIC)

        # Load the just-packed entries to compute the canonical hash.
        # Use the in-memory bytes so the hash math is identical to what
        # the agent does at unpack time.
        raw = tmp_archive.read_bytes()
        entries: dict[str, bytes] = {}
        with zipfile.ZipFile(io.BytesIO(raw)) as zf:
            for info in zf.infolist():
                if info.filename.endswith("/"):
                    continue
                entries[info.filename] = zf.read(info.filename)

        if MANIFEST_FILENAME not in entries:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"packed archive missing {MANIFEST_FILENAME}",
            )
            sys.exit(EXIT_GENERIC)

        payload_hash = _canonical_payload_hash(entries)

        # Load the Ed25519 private key. PEM is the canonical format; we
        # also accept the raw 32-byte form for parity with the legacy
        # signing shell script so existing keys keep working.
        try:
            from cryptography.hazmat.primitives.asymmetric.ed25519 import (
                Ed25519PrivateKey,
            )
            from cryptography.hazmat.primitives.serialization import (
                load_pem_private_key,
            )
        except ImportError as exc:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"cryptography package required: {exc}",
                hint="pip install cryptography",
            )
            sys.exit(EXIT_GENERIC)

        key_bytes = Path(key_path).read_bytes()
        try:
            private = load_pem_private_key(key_bytes, password=None)
        except ValueError:
            try:
                private = Ed25519PrivateKey.from_private_bytes(
                    key_bytes[:32]
                )
            except Exception as exc:  # noqa: BLE001
                _emit_err(
                    as_json,
                    EXIT_GENERIC,
                    f"could not load private key from {key_path}: {exc}",
                )
                sys.exit(EXIT_GENERIC)

        if not isinstance(private, Ed25519PrivateKey):
            _emit_err(
                as_json,
                EXIT_GENERIC,
                "private key is not an Ed25519 key",
            )
            sys.exit(EXIT_GENERIC)

        signature = private.sign(payload_hash)
        sig_b64 = base64.b64encode(signature).decode("ascii")

        # Write the final archive: original entries plus SIGNATURE.
        # Re-pack from scratch (rather than mutating the existing zip
        # in place) so the byte stream is deterministic.
        if output.exists():
            output.unlink()
        with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.writestr(
                MANIFEST_FILENAME, entries[MANIFEST_FILENAME]
            )
            for name in sorted(entries):
                if name in (MANIFEST_FILENAME, SIGNATURE_FILENAME):
                    continue
                zf.writestr(name, entries[name])
            zf.writestr(
                SIGNATURE_FILENAME, f"{signer_id}\n{sig_b64}\n"
            )

        sha256 = hashlib.sha256(output.read_bytes()).hexdigest()
        sums_path = output.with_suffix(output.suffix + ".sha256")
        sums_path.write_text(
            f"{sha256}  {output.name}\n", encoding="utf-8"
        )

        result = {
            "plugin_id": manifest.id,
            "version": manifest.version,
            "signer_id": signer_id,
            "signature_b64": sig_b64,
            "payload_hash_hex": payload_hash.hex(),
            "archive": str(output),
            "sha256": sha256,
            "sha256_file": str(sums_path),
        }
        if as_json:
            _emit_ok(as_json, result)
        else:
            click.echo(f"Plugin:    {manifest.id} v{manifest.version}")
            click.echo(f"Signer:    {signer_id}")
            click.echo(f"Signature: {sig_b64}")
            click.echo(f"SHA-256:   {sha256}")
            click.echo(f"Archive:   {output}")
            click.echo(f"Checksum:  {sums_path}")
    finally:
        if tmp_archive.exists():
            tmp_archive.unlink()


@plugin_group.command(
    "keygen",
    help=(
        "Generate a fresh Ed25519 keypair for plugin signing. "
        "Developer aid: do NOT use this to mint production publisher "
        "keys without an offline workflow for the private half."
    ),
)
@click.argument("signer_id")
@click.option(
    "--output-dir",
    "output_dir",
    required=True,
    type=click.Path(file_okay=False),
    help="Directory where <signer-id>.pem and <signer-id>.priv.pem will be written.",
)
@click.option(
    "--force",
    is_flag=True,
    help="Overwrite existing key files at the target paths.",
)
@click.option("--json", "as_json", is_flag=True)
def keygen(
    signer_id: str, output_dir: str, force: bool, as_json: bool
) -> None:
    """Mint a fresh Ed25519 keypair.

    Writes two files under ``output_dir``:

    * ``<signer-id>.pem`` — public key in PEM (mode 0644)
    * ``<signer-id>.priv.pem`` — private key in PEM (mode 0600)

    The public PEM is what installs onto the agent at
    ``/etc/ados/plugin-keys/<signer-id>.pem``. The private half stays
    with the signing rig — never check it in, never copy it across hosts
    without an encrypted transport. The developer-aid framing matters:
    production publisher keys deserve a hardware-token or air-gapped
    workflow, not a one-liner.
    """
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import (
            Ed25519PrivateKey,
        )
        from cryptography.hazmat.primitives.serialization import (
            Encoding,
            NoEncryption,
            PrivateFormat,
            PublicFormat,
        )
    except ImportError as exc:
        _emit_err(
            as_json,
            EXIT_GENERIC,
            f"cryptography package required: {exc}",
            hint="pip install cryptography",
        )
        sys.exit(EXIT_GENERIC)

    out_dir = Path(output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    pub_path = out_dir / f"{signer_id}.pem"
    priv_path = out_dir / f"{signer_id}.priv.pem"

    for path in (pub_path, priv_path):
        if path.exists() and not force:
            _emit_err(
                as_json,
                EXIT_GENERIC,
                f"{path} already exists",
                hint="Pass --force to overwrite, or pick a different signer-id.",
            )
            sys.exit(EXIT_GENERIC)

    private = Ed25519PrivateKey.generate()
    public = private.public_key()

    priv_pem = private.private_bytes(
        encoding=Encoding.PEM,
        format=PrivateFormat.PKCS8,
        encryption_algorithm=NoEncryption(),
    )
    pub_pem = public.public_bytes(
        encoding=Encoding.PEM,
        format=PublicFormat.SubjectPublicKeyInfo,
    )

    pub_path.write_bytes(pub_pem)
    pub_path.chmod(0o644)
    priv_path.write_bytes(priv_pem)
    priv_path.chmod(0o600)

    # Fingerprint = SHA-256 of the raw public-key bytes, base64 encoded
    # short-form. Stable, copy-pasteable identifier for the operator to
    # cross-check against what's installed on the agent.
    import base64
    import hashlib

    raw_pub = public.public_bytes(
        encoding=Encoding.Raw, format=PublicFormat.Raw
    )
    fp = base64.b64encode(hashlib.sha256(raw_pub).digest()).decode(
        "ascii"
    )[:22]

    result = {
        "signer_id": signer_id,
        "public_key_path": str(pub_path),
        "private_key_path": str(priv_path),
        "fingerprint": fp,
    }
    if as_json:
        _emit_ok(as_json, result)
    else:
        click.echo(f"Signer ID:   {signer_id}")
        click.echo(f"Public key:  {pub_path}")
        click.echo(f"Private key: {priv_path} (mode 0600)")
        click.echo(f"Fingerprint: {fp}")
        click.echo("")
        click.echo(
            "Install the public key on the agent at "
            f"/etc/ados/plugin-keys/{signer_id}.pem"
        )
        click.echo(
            "and keep the private key offline. Never commit *.priv.pem to git."
        )
