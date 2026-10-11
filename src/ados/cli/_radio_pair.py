"""``ados radio pair`` subcommand tree."""

from __future__ import annotations

import json
from typing import Any

import click

from ados.cli._radio_base import _request, radio_group

# ---------------------------------------------------------------------
# Pair lifecycle: status, local bind, unpair, auto-pair toggle.
# ---------------------------------------------------------------------


@click.group("pair", help="Inspect and control WFB radio pairing.")
def pair_group() -> None:
    pass


radio_group.add_command(pair_group)


def _print_pair_status(data: dict[str, Any]) -> None:
    paired = bool(data.get("paired"))
    state = "Paired" if paired else "Not paired"
    click.echo(click.style(f"Pair state: {state}", bold=True))
    click.echo(f"  Role            {data.get('role') or '—'}")
    click.echo(f"  Peer device-id  {data.get('paired_with_device_id') or '—'}")
    click.echo(f"  Paired at       {data.get('paired_at') or '—'}")
    click.echo(f"  Fingerprint     {data.get('fingerprint') or '—'}")
    click.echo(
        f"  Auto-pair       "
        f"{'armed' if data.get('auto_pair_enabled') else 'disabled'}"
    )


@pair_group.command("status", help="Print pair state for this rig.")
@click.option("--json", "as_json", is_flag=True)
def pair_status(as_json: bool) -> None:
    _, body = _request("GET", "/api/wfb/pair")
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
    else:
        _print_pair_status(body)


@pair_group.command(
    "local",
    help=(
        "Open a local-radio bind window. Drives the upstream wfb-ng bind "
        "protocol via the agent's REST. Returns the terminal session "
        "state when the protocol completes (≤60s)."
    ),
)
@click.option(
    "--role",
    type=click.Choice(["drone", "gs"]),
    default=None,
    help="Override the inferred profile role.",
)
@click.option(
    "--peer-device-id",
    "peer_device_id",
    default=None,
    help="Optional peer device-id to persist alongside pair state.",
)
def pair_local(role: str | None, peer_device_id: str | None) -> None:
    payload: dict[str, Any] = {}
    if role is not None:
        payload["role"] = role
    if peer_device_id is not None:
        payload["peer_device_id"] = peer_device_id
    code, body = _request(
        "POST",
        "/api/wfb/pair/local-bind",
        json=payload,
        raise_for_status=False,
        timeout=120.0,
    )
    if code == 409:
        click.echo(
            click.style("A bind session is already in progress.", fg="yellow")
        )
        raise click.exceptions.Exit(code=2)
    if code >= 400:
        click.echo(click.style(f"Bind failed: HTTP {code}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)

    state = body.get("state")
    if state == "paired":
        click.echo(click.style("Bind succeeded.", fg="green"))
        click.echo(f"  fingerprint   {body.get('fingerprint')}")
        click.echo(f"  finished_at   {body.get('finished_at')}")
    else:
        click.echo(
            click.style(
                f"Bind ended in state '{state}': {body.get('error') or '—'}",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)


@pair_group.command(
    "failover-status",
    help="Show local-bind to cloud-relay failover state.",
)
@click.option("--json", "as_json", is_flag=True, help="Emit JSON.")
def pair_failover_status(as_json: bool) -> None:
    _, body = _request("GET", "/api/wfb/pair/failover-status")
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
        return
    state = body.get("failover_state", "local")
    if state == "local":
        click.echo("Local bind active.")
    elif state == "cloud_relay":
        click.echo(
            "Failover to cloud relay. "
            "Run `ados radio pair auto on` to retry local bind."
        )
    elif state == "failed":
        click.echo("Pairing failed.")
    else:
        click.echo(f"Unknown state: {state}")


@pair_group.command("unpair", help="Wipe pair keys and clear pair state.")
@click.option("--yes", is_flag=True, help="Skip the confirmation prompt.")
def pair_unpair(yes: bool) -> None:
    if not yes:
        click.confirm(
            "Wipe wfb-ng key files and clear pair state?",
            abort=True,
            default=False,
        )
    _, body = _request("POST", "/api/wfb/pair/unpair")
    click.echo("Unpaired.")
    if isinstance(body, dict) and body.get("role"):
        click.echo(f"  role  {body['role']}")


@pair_group.group("auto", help="Toggle auto-pair on first boot.")
def pair_auto_group() -> None:
    pass


@pair_auto_group.command("on", help="Re-arm auto-pair (only when unpaired).")
def pair_auto_on() -> None:
    code, body = _request(
        "PUT",
        "/api/wfb/pair/auto-pair",
        json={"enabled": True},
        raise_for_status=False,
    )
    if code >= 400:
        click.echo(click.style(f"Failed: HTTP {code}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)
    if body.get("rearm_blocked"):
        click.echo(
            click.style(
                "Cannot re-arm auto-pair while paired. "
                "Run `ados radio pair unpair` first.",
                fg="yellow",
            )
        )
        raise click.exceptions.Exit(code=2)
    click.echo("Auto-pair armed.")


@pair_auto_group.command("off", help="Disable auto-pair.")
def pair_auto_off() -> None:
    _, _body = _request(
        "PUT",
        "/api/wfb/pair/auto-pair",
        json={"enabled": False},
        raise_for_status=False,
    )
    click.echo("Auto-pair disabled.")
