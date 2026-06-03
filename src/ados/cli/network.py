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
from datetime import datetime, timezone
from pathlib import Path

import click

from ados.cli.radio import _request
from ados.core.paths import CONFIG_YAML

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


# --- Operating region (regulatory posture) -----------------------------------
#
# The radio ships unrestricted by default: it brings up and transmits on the
# configured home channel without first verifying a regional domain (the
# operator is responsible for legal RF operation in their jurisdiction). An
# operator who wants their jurisdiction's channel set and per-channel power
# limit enforced pins a region (an ISO 3166-1 alpha-2 country code). These
# commands write ``network.regulatory.*`` straight to config.yaml so the
# choice is durable and reproducible from the repo; the radio re-reads the
# posture on its next restart.


def _read_config_yaml() -> dict:
    path = Path(CONFIG_YAML)
    if not path.exists():
        return {}
    try:
        import yaml

        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        return {}
    except ImportError as exc:
        raise click.ClickException("pyyaml is required to read config.yaml") from exc
    return data if isinstance(data, dict) else {}


def _write_regulatory(mode: str, region: str | None) -> None:
    """Persist ``network.regulatory.*`` to config.yaml without touching unrelated keys."""
    try:
        import yaml
    except ImportError as exc:
        raise click.ClickException("pyyaml is required to update config.yaml") from exc

    path = Path(CONFIG_YAML)
    data = _read_config_yaml()
    network = data.setdefault("network", {})
    if not isinstance(network, dict):
        network = {}
        data["network"] = network
    reg = network.setdefault("regulatory", {})
    if not isinstance(reg, dict):
        reg = {}
        network["regulatory"] = reg

    reg["mode"] = mode
    reg["region"] = region
    reg["ack_operator"] = reg.get("ack_operator") or "cli"
    reg["ack_at"] = (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )

    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(
        yaml.safe_dump(data, sort_keys=False, default_flow_style=False),
        encoding="utf-8",
    )
    tmp.replace(path)


@network_group.group(
    "regulatory",
    help="Operating-region posture for the radio (unrestricted by default).",
)
def regulatory_group() -> None:
    pass


@regulatory_group.command("status", help="Show the current operating-region posture.")
@click.option("--json", "as_json", is_flag=True, help="Emit raw JSON.")
def regulatory_status(as_json: bool) -> None:
    data = _read_config_yaml()
    reg = ((data.get("network") or {}).get("regulatory") or {}) if isinstance(data, dict) else {}
    if not isinstance(reg, dict):
        reg = {}
    mode = str(reg.get("mode") or "unrestricted")
    region = reg.get("region")
    if as_json:
        click.echo(
            json.dumps(
                {
                    "mode": mode,
                    "region": region if isinstance(region, str) else None,
                    "ack_operator": reg.get("ack_operator"),
                    "ack_at": reg.get("ack_at"),
                },
                indent=2,
            )
        )
        return
    if mode == "region" and isinstance(region, str) and region:
        click.echo(f"Operating region: pinned to {region.upper()}")
        click.echo("The radio enforces this region's channel set and power limits.")
    else:
        click.echo("Operating region: unrestricted (default)")
        click.echo("You are responsible for legal RF operation in your jurisdiction.")


@regulatory_group.command(
    "unrestricted",
    help="Clear any pinned region; radiate on the configured channel out of the box.",
)
def regulatory_unrestricted() -> None:
    _write_regulatory("unrestricted", None)
    click.echo("Operating region set to unrestricted.")
    click.echo("Restart the agent (or reboot) to apply.")


@regulatory_group.command(
    "region",
    help="Pin an operating region (ISO 3166-1 alpha-2 country code, e.g. US, DE).",
)
@click.argument("code")
def regulatory_region(code: str) -> None:
    region = (code or "").strip().upper()
    if not (len(region) == 2 and region.isalpha()):
        raise click.ClickException(
            f"region must be a 2-letter country code (e.g. US, DE, GB), got '{code}'"
        )
    _write_regulatory("region", region)
    click.echo(f"Operating region pinned to {region}.")
    click.echo("Restart the agent (or reboot) to apply.")


__all__ = ["network_group"]
