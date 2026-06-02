"""``ados network`` CLI: inspect and manage stable-MAC pinning.

Some onboard USB WiFi chipsets have no efuse MAC and randomize their address
each boot, churning the DHCP lease (and the box's IP). The agent auto-pins a
deterministic stable MAC for such a chipset; these commands show the verdicts
and let an operator confirm a learner candidate, set an explicit override, or
unpin.

``status`` reads the agent state file directly so it works on the bench even
when the API is down; ``pin`` / ``unpin`` go through the local REST API (so the
override is validated + persisted through the running config); ``verify`` runs
``udevadm`` to show which ``.link`` currently wins for an interface.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import click

from ados.cli.radio import _request

_STATE_PATH = Path("/etc/ados/mac-pins.state")


@click.group("network", help="Inspect and manage network adapter settings.")
def network_group() -> None:
    pass


@network_group.group("mac", help="Stable-MAC pinning for no-efuse adapters.")
def mac_group() -> None:
    pass


def _read_state() -> dict:
    try:
        return json.loads(_STATE_PATH.read_text())
    except (OSError, ValueError):
        return {}


@mac_group.command("status", help="Show per-adapter stable-MAC verdicts.")
@click.option("--json", "as_json", is_flag=True, help="Emit raw JSON.")
def mac_status(as_json: bool) -> None:
    state = _read_state()
    adapters = [a for a in (state.get("adapters") or []) if isinstance(a, dict)]
    if as_json:
        click.echo(json.dumps(state, indent=2))
        return
    if not adapters:
        click.echo("No network adapters tracked (no no-efuse randomizer detected).")
        return
    click.echo(f"{'IFACE':<16} {'VID:PID':<10} {'STATE':<10} {'SOURCE':<9} PINNED MAC")
    for a in adapters:
        click.echo(
            f"{(a.get('name') or '-'):<16} "
            f"{(a.get('vidpid') or '-'):<10} "
            f"{(a.get('state') or '-'):<10} "
            f"{(a.get('source') or '-'):<9} "
            f"{a.get('pinned_mac') or '-'}"
        )
        if a.get("deferred_reason"):
            click.echo(f"    deferred: {a['deferred_reason']}")
    click.echo("\nTip: add a DHCP reservation for a pinned MAC to fix the IP too.")


@mac_group.command("pin", help="Pin a stable MAC on an adapter.")
@click.argument("iface")
@click.option("--mac", "mac", default=None, help="Explicit MAC (else the proposed value).")
@click.option(
    "--apply-now",
    is_flag=True,
    help="Also re-tag the LIVE interface now (drops connections over it).",
)
def mac_pin(iface: str, mac: str | None, apply_now: bool) -> None:
    payload: dict = {"iface": iface, "mac": mac, "apply_now": apply_now}
    _code, body = _request("POST", "/api/v1/network/mac/pin", json=payload)
    click.echo(
        f"iface={body.get('iface')} mac={body.get('mac')} "
        f"appliedLive={body.get('appliedLive')}"
    )
    if body.get("note"):
        click.echo(body["note"])


@mac_group.command("unpin", help="Remove an adapter's pin / override.")
@click.argument("iface")
def mac_unpin(iface: str) -> None:
    _code, body = _request("DELETE", f"/api/v1/network/mac/{iface}")
    click.echo(
        f"removedOverride={body.get('removedOverride')} "
        f"removedLinkFile={body.get('removedLinkFile')}"
    )
    if body.get("note"):
        click.echo(body["note"])


@mac_group.command("verify", help="Show which .link wins for an interface (no reboot).")
@click.argument("iface")
def mac_verify(iface: str) -> None:
    try:
        out = subprocess.run(
            ["udevadm", "test-builtin", "net_setup_link", f"/sys/class/net/{iface}"],
            capture_output=True,
            text=True,
            timeout=8,
        )
    except Exception as exc:  # noqa: BLE001
        raise click.ClickException(f"udevadm failed: {exc}") from exc
    text = (out.stdout or "") + (out.stderr or "")
    shown = False
    for line in text.splitlines():
        if "is applied" in line or "MACAddress" in line:
            click.echo(line.strip())
            shown = True
    if not shown:
        click.echo(f"no .link verdict found for {iface}")


__all__ = ["network_group"]
