"""``ados mcp`` CLI subcommand tree.

Manage the scoped, revocable tokens an MCP client (the ADOS-MCP connector in
agent-mode) presents instead of the full pairing key:

* ``ados mcp status`` — show the acceptance posture + the minted tokens.
* ``ados mcp mint`` — mint a scoped token (shown once).
* ``ados mcp revoke`` — revoke one token by id, or ``--all``.

All three drive the agent's REST API at ``http://localhost:8080``; on-box the
mint/status/revoke routes are admitted without a key (loopback trust), so the
CLI behaves identically to the GCS hitting the same routes with the key.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import click
import httpx

from ados.core.paths import PAIRING_JSON

API_BASE = "http://localhost:8080"

# The scope groups a token may carry. Mirrors the agent's route->scope classes.
_VALID_SCOPES = ("read", "safe_write", "admin", "flight", "destructive", "secret_read")


def _load_api_key() -> str | None:
    """The pairing key, or None when unpaired / unreadable (on-box is trusted)."""
    try:
        data = json.loads(Path(PAIRING_JSON).read_text())
    except (OSError, ValueError):
        return None
    key = data.get("api_key")
    return key if isinstance(key, str) and key else None


def _auth_headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _request(method: str, path: str, **kwargs: Any) -> tuple[int, dict[str, Any]]:
    """Minimal REST helper. Returns (status_code, parsed_body)."""
    try:
        with httpx.Client(timeout=kwargs.pop("timeout", 8.0)) as client:
            resp = client.request(
                method, f"{API_BASE}{path}", headers=_auth_headers(), **kwargs
            )
            try:
                body = resp.json()
            except ValueError:
                body = {"text": resp.text}
            if resp.status_code >= 400:
                detail = body.get("detail") if isinstance(body, dict) else None
                raise click.ClickException(
                    f"Agent API returned {resp.status_code}: {detail or body}"
                )
            return resp.status_code, body if isinstance(body, dict) else {"data": body}
    except httpx.ConnectError as exc:
        raise click.ClickException(
            "Agent is not running on http://localhost:8080. "
            "Start the supervisor or run `ados demo`."
        ) from exc
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc


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
