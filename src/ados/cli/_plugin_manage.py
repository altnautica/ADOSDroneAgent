"""``ados plugin`` lifecycle: list, install, enable, disable, remove, perms."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

import click

from ados.cli._plugin_common import (
    EXIT_GENERIC,
    EXIT_NOT_FOUND,
    EXIT_OK,
    EXIT_PERMISSION_DENIED,
    _agent,
    _call,
    _emit_ok,
    _fail,
    _failure,
    _is_builtin_install,
    _seg,
    plugin_group,
)
from ados.plugins.builtin import builtin_manifest, builtin_manifests


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
