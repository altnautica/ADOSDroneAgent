"""`ados signing ...` subcommands for MAVLink signing.

The agent does not own the signing key. These CLI commands let an
operator on the SBC see whether the connected FC supports signing,
read counters of signed frames passing through the agent, and in the
worst case clear the FC's signing store if every GCS copy of the key
has been lost.

Registered under the main `cli` group via `cli.add_command(signing_group)`.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import click
import httpx

API_BASE = "http://127.0.0.1:8080"
PAIRING_STATE_PATH = Path("/etc/ados/pairing.json")


def _load_api_key() -> str | None:
    try:
        if PAIRING_STATE_PATH.exists():
            with open(PAIRING_STATE_PATH) as f:
                data = json.load(f)
            key = data.get("api_key")
            return str(key) if key else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _get(path: str) -> dict[str, Any] | None:
    try:
        resp = httpx.get(f"{API_BASE}{path}", headers=_headers(), timeout=10.0)
    except httpx.HTTPError as exc:
        click.echo(f"error: agent not reachable: {exc}", err=True)
        return None
    if resp.status_code >= 400:
        click.echo(f"error: HTTP {resp.status_code}: {resp.text}", err=True)
        return None
    try:
        return resp.json()
    except ValueError:
        click.echo(f"error: non-JSON response: {resp.text}", err=True)
        return None


def _post(path: str) -> dict[str, Any] | None:
    try:
        resp = httpx.post(f"{API_BASE}{path}", headers=_headers(), timeout=10.0)
    except httpx.HTTPError as exc:
        click.echo(f"error: agent not reachable: {exc}", err=True)
        return None
    if resp.status_code >= 400:
        click.echo(f"error: HTTP {resp.status_code}: {resp.text}", err=True)
        return None
    try:
        return resp.json()
    except ValueError:
        return None


# ---------------------------------------------------------------------------
# Click group
# ---------------------------------------------------------------------------


@click.group(name="signing")
def signing_group() -> None:
    """MAVLink signing capability + FC recovery commands."""


@signing_group.command(name="capability")
def cmd_capability() -> None:
    """Print whether the connected FC supports MAVLink signing."""
    data = _get("/api/mavlink/signing/capability")
    if data is None:
        sys.exit(1)

    if data.get("supported"):
        fw = data.get("firmware_name") or "ArduPilot"
        click.echo(f"supported: yes ({fw})")
        click.echo(f"signing params on FC: {data.get('signing_params_present')}")
    else:
        reason = data.get("reason") or "unknown"
        fw = data.get("firmware_name") or "unknown"
        click.echo(f"supported: no")
        click.echo(f"reason:    {reason}")
        click.echo(f"firmware:  {fw}")


@signing_group.command(name="counters")
def cmd_counters() -> None:
    """Print signed-frame counters observed by the agent."""
    data = _get("/api/mavlink/signing/counters")
    if data is None:
        sys.exit(1)
    click.echo(f"tx signed frames: {data.get('tx_signed_count', 0)}")
    click.echo(f"rx signed frames: {data.get('rx_signed_count', 0)}")
    last = data.get("last_signed_rx_at")
    if last:
        click.echo(f"last signed rx:   {last}")
    else:
        click.echo("last signed rx:   never")


@signing_group.command(name="clear-fc")
def cmd_clear_fc() -> None:
    """Clear the FC's signing store. Emergency recovery only.

    Sends SETUP_SIGNING with an all-zero key. The FC will accept unsigned
    commands again. Use this only if every GCS copy of the signing key
    has been lost and you cannot enroll a fresh key via the normal flow.
    """
    click.echo("This will clear the FC's MAVLink signing store.")
    click.echo("The FC will accept unsigned commands until a new key is enrolled.")
    click.echo("")
    confirm = click.prompt("Type CLEAR to confirm", default="", show_default=False)
    if confirm != "CLEAR":
        click.echo("aborted.")
        sys.exit(2)

    data = _post("/api/mavlink/signing/disable-on-fc")
    if data is None:
        sys.exit(1)
    click.echo("ok: FC signing cleared.")


@signing_group.command(name="require")
@click.argument("value", required=False, default=None, type=click.Choice(["on", "off"]))
def cmd_require(value: str | None) -> None:
    """Read or set SIGNING_REQUIRE on the FC.

    With no argument: prints the current value. With `on` or `off`: writes
    the new value to the FC param store.
    """
    if value is None:
        data = _get("/api/mavlink/signing/require")
        if data is None:
            sys.exit(1)
        current = data.get("require")
        if current is None:
            click.echo("SIGNING_REQUIRE: unknown (param not cached yet)")
        else:
            click.echo(f"SIGNING_REQUIRE: {'on' if current else 'off'}")
        return

    require_bool = value == "on"
    try:
        resp = httpx.put(
            f"{API_BASE}/api/mavlink/signing/require",
            headers={**_headers(), "Content-Type": "application/json"},
            json={"require": require_bool},
            timeout=10.0,
        )
    except httpx.HTTPError as exc:
        click.echo(f"error: agent not reachable: {exc}", err=True)
        sys.exit(1)
    if resp.status_code >= 400:
        click.echo(f"error: HTTP {resp.status_code}: {resp.text}", err=True)
        sys.exit(1)
    click.echo(f"ok: SIGNING_REQUIRE set to {value}")
