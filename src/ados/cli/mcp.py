"""``ados mcp`` CLI subcommand tree.

Manage the scoped, revocable tokens an MCP client (the ADOS-MCP connector in
agent-mode) presents instead of the full pairing key:

* ``ados mcp status`` — show the acceptance posture + the minted tokens.
* ``ados mcp mint`` — mint a scoped token (shown once).
* ``ados mcp revoke`` — revoke one token by id, or ``--all``.
* ``ados mcp enable`` / ``ados mcp disable`` — turn scoped-token acceptance at the
  LAN auth edge on/off (writes ``mcp.token_accept_enabled`` to ``config.yaml`` and
  restarts ``ados-control`` so the change takes effect). Needs ``sudo``.

The status/mint/revoke commands drive the agent's REST API at
``http://localhost:8080``; on-box the mint/status/revoke routes are admitted
without a key (loopback trust). enable/disable edit the on-box config the control
front reads at startup, so they run locally with root privilege.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import click

from ados.cli import _agent_http
from ados.core.paths import CONFIG_YAML

# The scope groups a token may carry. Mirrors the agent's route->scope classes.
_VALID_SCOPES = ("read", "safe_write", "admin", "flight", "destructive", "secret_read")


def _request(method: str, path: str, **kwargs: Any) -> tuple[int, dict[str, Any]]:
    """One control-surface request: ``(status, body)``; an error status raises."""
    response = _agent_http.call(
        method, path, json_body=kwargs.get("json"), timeout=kwargs.get("timeout", 8.0)
    )
    body = response.body
    if not response.ok:
        detail = body.get("detail") if isinstance(body, dict) else None
        raise click.ClickException(f"Agent API returned {response.status}: {detail or body}")
    return response.status, body if isinstance(body, dict) else {"data": body}


@click.group("mcp", help="Manage AI-control (MCP) access tokens.")
def mcp_group() -> None:
    """Command group for the MCP token surface."""


@mcp_group.command("status", help="Show the MCP token acceptance posture and minted tokens.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def mcp_status(as_json: bool) -> None:
    _, data = _request("GET", "/api/mcp/status")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return
    enabled = bool(data.get("accept_enabled"))
    click.echo(click.style("MCP access", bold=True))
    posture = (
        "enabled"
        if enabled
        else "disabled (tokens can be minted but are not honored at the edge yet)"
    )
    click.echo(f"  token acceptance: {posture}")
    tokens = data.get("tokens") or []
    if not tokens:
        click.echo("  no tokens minted")
        return
    click.echo(f"  {len(tokens)} token(s):")
    for t in tokens:
        flags = [f for f, on in (("revoked", t.get("revoked")), ("expired", t.get("expired"))) if on]
        suffix = f"  [{', '.join(flags)}]" if flags else ""
        label = t.get("label") or "(no label)"
        scopes = ",".join(t.get("scopes") or [])
        click.echo(f"    {t.get('token_id')}  {label}  scopes={scopes}{suffix}")


@mcp_group.command("mint", help="Mint a scoped MCP token (shown once).")
@click.option("--label", default="", help="A human label for the token.")
@click.option(
    "--scope",
    "scopes",
    multiple=True,
    required=True,
    type=click.Choice(_VALID_SCOPES),
    help="A scope group to grant (repeatable).",
)
@click.option("--ttl-days", type=int, default=30, show_default=True, help="Lifetime in days.")
@click.option(
    "--node",
    "allowed_nodes",
    multiple=True,
    help=(
        "Advisory node allowlist carried in the token (repeatable). This token is "
        "already bound to THIS node by its issuer; the list is a hint for a fleet "
        "connector, not an extra agent-side restriction."
    ),
)
def mcp_mint(
    label: str, scopes: tuple[str, ...], ttl_days: int, allowed_nodes: tuple[str, ...]
) -> None:
    ttl_ms = max(1, ttl_days) * 24 * 60 * 60 * 1000
    payload = {
        "label": label,
        "scopes": list(scopes),
        "allowed_nodes": list(allowed_nodes),
        "ttl_ms": ttl_ms,
    }
    _, data = _request("POST", "/api/mcp/tokens", json=payload)
    token = data.get("token")
    if not token:
        raise click.ClickException(f"Mint failed: {data}")
    click.echo(click.style("Token (shown once — copy it now):", bold=True))
    click.echo(token)


@mcp_group.command("revoke", help="Revoke one token by id, or --all.")
@click.argument("token_id", required=False)
@click.option("--all", "revoke_all", is_flag=True, help="Revoke every token (salt rotation).")
def mcp_revoke(token_id: str | None, revoke_all: bool) -> None:
    if revoke_all:
        payload: dict[str, Any] = {"all": True}
    elif token_id:
        payload = {"token_id": token_id}
    else:
        raise click.ClickException("Provide a token id, or --all.")
    _request("POST", "/api/mcp/revoke", json=payload)
    click.echo("All MCP tokens revoked." if revoke_all else "Revoked.")


# --- acceptance toggle (config write + control restart) ---------------------
#
# ``mcp.token_accept_enabled`` lives in the on-box config the control front reads
# at startup, so turning acceptance on/off is a config write plus a restart of
# ``ados-control`` — the same shape as ``ados profile set``. It is a local root
# operation (writes ``/etc/ados/config.yaml``, restarts a unit), NOT a REST call,
# so it must be run with ``sudo``. Never hand-edit the config file — this is the
# sanctioned path so the change is reproducible.

CONTROL_UNIT = "ados-control"


def _write_token_accept(enabled: bool) -> Path:
    """Set ``mcp.token_accept_enabled`` in config.yaml, leaving other keys intact.

    Idempotent. Goes through ``ados.core.config.writer``, the one config
    writer, so the flag lands as a merge under the same flock every other
    writer takes — an operator flipping this while the agent is serving a
    settings PUT can no longer lose either write.
    """
    from ados.core.config.writer import set_config_values

    path = Path(CONFIG_YAML)
    result = set_config_values({"mcp.token_accept_enabled": enabled}, path=path)
    if not result:
        raise click.ClickException(
            f"Could not update {path}: {result.error}\n"
            f"If this is a permission failure, re-run with sudo: "
            f"`sudo ados mcp {'enable' if enabled else 'disable'}`."
        )
    return path


def _restart_control() -> tuple[bool, str]:
    """Restart the control front so the new acceptance posture takes effect."""
    try:
        result = subprocess.run(
            ["systemctl", "restart", CONTROL_UNIT],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except FileNotFoundError:
        return False, "systemctl not found (not a systemd host)"
    except subprocess.TimeoutExpired:
        return False, "restart timed out"
    if result.returncode != 0:
        return False, (result.stderr or result.stdout or "restart failed").strip()
    return True, ""


def _apply_token_accept(enabled: bool, no_restart: bool) -> None:
    """Write the flag and (unless suppressed) restart the control front."""
    path = _write_token_accept(enabled)
    state = "enabled" if enabled else "disabled"
    click.echo(f"MCP token acceptance {state} in {path}.")
    if no_restart:
        click.echo(
            f"Restart the control front to apply: `sudo systemctl restart {CONTROL_UNIT}`."
        )
        return
    ok, detail = _restart_control()
    if ok:
        click.echo(f"Restarted {CONTROL_UNIT}; the new posture is live.")
    else:
        click.echo(
            click.style(
                f"Config written, but restarting {CONTROL_UNIT} failed: {detail}\n"
                f"Apply it with `sudo systemctl restart {CONTROL_UNIT}`.",
                fg="yellow",
            )
        )


@mcp_group.command("enable", help="Accept scoped MCP tokens at the LAN auth edge (needs sudo).")
@click.option("--no-restart", is_flag=True, help="Write the config but do not restart the control front.")
def mcp_enable(no_restart: bool) -> None:
    _apply_token_accept(True, no_restart)


@mcp_group.command("disable", help="Stop honoring scoped MCP tokens at the LAN auth edge (needs sudo).")
@click.option("--no-restart", is_flag=True, help="Write the config but do not restart the control front.")
def mcp_disable(no_restart: bool) -> None:
    _apply_token_accept(False, no_restart)
