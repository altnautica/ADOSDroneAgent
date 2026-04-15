"""`ados gs ...` subcommands for the ground-station profile.

Wave C Cellos (MSN-025 Phase 1). Wraps the REST API exposed by
`ados.api.routes.ground_station` so bench operators can drive the
ground-station agent from the local shell without curl gymnastics.

Registered under the main `cli` group in `ados.cli.main` via
`cli.add_command(gs_group)`.
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


# ---------------------------------------------------------------------------
# Auth + transport helpers.
# ---------------------------------------------------------------------------


def _load_api_key() -> str | None:
    """Read /etc/ados/pairing.json for the X-ADOS-Key value."""
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


def _handle_resp(resp: httpx.Response) -> dict[str, Any] | None:
    """Pretty-print API errors and return parsed JSON on success."""
    if resp.status_code >= 400:
        try:
            body = resp.json()
        except ValueError:
            body = {"raw": resp.text}
        click.echo(
            f"Error {resp.status_code}: {json.dumps(body, indent=2)}",
            err=True,
        )
        return None
    try:
        return resp.json()
    except ValueError:
        click.echo("Error: invalid JSON response from agent.", err=True)
        return None


def _request(
    method: str,
    path: str,
    *,
    json_body: dict[str, Any] | None = None,
    params: dict[str, Any] | None = None,
) -> dict[str, Any] | None:
    url = f"{API_BASE}{path}"
    try:
        resp = httpx.request(
            method,
            url,
            headers=_headers(),
            json=json_body,
            params=params,
            timeout=10.0,
        )
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as exc:
        click.echo(f"Error: {exc}", err=True)
        return None
    return _handle_resp(resp)


def _pp(data: Any) -> None:
    click.echo(json.dumps(data, indent=2, sort_keys=True))


# ---------------------------------------------------------------------------
# Group: `ados gs`
# ---------------------------------------------------------------------------


@click.group("gs")
def gs_group() -> None:
    """Ground-station controls: status, pair, network, UI, factory reset."""


# ── status / pair / unpair / reset ─────────────────────────────────────────


@gs_group.command("status")
def gs_status() -> None:
    """Fetch the full ground-station status snapshot."""
    data = _request("GET", "/api/v1/ground-station/status")
    if data is not None:
        _pp(data)


@gs_group.command("pair")
@click.argument("pair_key")
@click.option(
    "--drone-id",
    "drone_device_id",
    default=None,
    help="Optional drone device id to associate with this pairing.",
)
def gs_pair(pair_key: str, drone_device_id: str | None) -> None:
    """Install a drone pair key. Fails with 409 if already paired."""
    body: dict[str, Any] = {"pair_key": pair_key}
    if drone_device_id:
        body["drone_device_id"] = drone_device_id
    data = _request("POST", "/api/v1/ground-station/wfb/pair", json_body=body)
    if data is not None:
        click.echo(
            f"Paired. drone={data.get('paired_drone_id')} "
            f"fingerprint={data.get('key_fingerprint')} "
            f"at={data.get('paired_at')}"
        )


@gs_group.command("unpair")
def gs_unpair() -> None:
    """Remove the installed pair key."""
    data = _request("DELETE", "/api/v1/ground-station/wfb/pair")
    if data is not None:
        click.echo(
            f"Unpaired. previous={data.get('previous_drone_id')}"
        )


@gs_group.command("reset")
@click.option(
    "--confirm",
    required=True,
    help="Current pair key fingerprint, or 'factory-reset-unpaired' if not paired.",
)
def gs_reset(confirm: str) -> None:
    """Factory reset the ground station."""
    data = _request(
        "POST",
        "/api/v1/ground-station/factory-reset",
        params={"confirm": confirm},
    )
    if data is not None:
        click.echo(f"Factory reset complete at {data.get('timestamp')}.")


# ── network ────────────────────────────────────────────────────────────────


@gs_group.group("network")
def gs_network() -> None:
    """Network-stack controls (AP, uplinks)."""


@gs_network.command("ap")
@click.option("--enabled", type=bool, default=None, help="Start or stop the AP.")
@click.option("--ssid", default=None, help="Set the AP SSID (must start with ADOS-GS-).")
@click.option("--passphrase", default=None, help="Set the WPA2 passphrase.")
@click.option("--channel", type=int, default=None, help="Set the 2.4 GHz channel.")
def gs_network_ap(
    enabled: bool | None,
    ssid: str | None,
    passphrase: str | None,
    channel: int | None,
) -> None:
    """View or update the AP config. No flags prints the current state."""
    any_change = any(
        v is not None for v in (enabled, ssid, passphrase, channel)
    )
    if not any_change:
        data = _request("GET", "/api/v1/ground-station/network")
        if data is not None:
            _pp(data.get("ap", {}))
        return

    body: dict[str, Any] = {}
    if enabled is not None:
        body["enabled"] = enabled
    if ssid is not None:
        body["ssid"] = ssid
    if passphrase is not None:
        body["passphrase"] = passphrase
    if channel is not None:
        body["channel"] = channel
    data = _request("PUT", "/api/v1/ground-station/network/ap", json_body=body)
    if data is not None:
        _pp(data)


# ── UI config ──────────────────────────────────────────────────────────────


@gs_group.group("ui")
def gs_ui() -> None:
    """UI config (OLED, buttons, screens)."""


@gs_ui.command("oled")
@click.option("--brightness", type=int, default=None, help="0-100.")
@click.option("--auto-dim", "auto_dim", type=bool, default=None, help="Enable auto-dim.")
@click.option(
    "--cycle",
    "cycle_seconds",
    type=int,
    default=None,
    help="Screen cycle time in seconds (1-60).",
)
def gs_ui_oled(
    brightness: int | None,
    auto_dim: bool | None,
    cycle_seconds: int | None,
) -> None:
    """View or update OLED settings. No flags prints current config."""
    any_change = any(
        v is not None for v in (brightness, auto_dim, cycle_seconds)
    )
    if not any_change:
        data = _request("GET", "/api/v1/ground-station/ui")
        if data is not None:
            _pp(data.get("oled", {}))
        return

    body: dict[str, Any] = {}
    if brightness is not None:
        body["brightness"] = brightness
    if auto_dim is not None:
        body["auto_dim_enabled"] = auto_dim
    if cycle_seconds is not None:
        body["screen_cycle_seconds"] = cycle_seconds
    data = _request("PUT", "/api/v1/ground-station/ui/oled", json_body=body)
    if data is not None:
        _pp(data.get("oled", data))


# ── display ────────────────────────────────────────────────────────────────


@gs_group.group("display")
def gs_display() -> None:
    """HDMI kiosk display config."""


@gs_display.command("get")
def gs_display_get() -> None:
    """Print the current display config."""
    data = _request("GET", "/api/v1/ground-station/display")
    if data is not None:
        _pp(data)


@gs_display.command("set")
@click.option(
    "--resolution",
    type=click.Choice(["auto", "720p", "1080p"]),
    default=None,
    help="HDMI output resolution.",
)
@click.option(
    "--kiosk-enabled",
    "kiosk_enabled",
    type=bool,
    default=None,
    help="Enable or disable the kiosk browser.",
)
@click.option(
    "--kiosk-url",
    "kiosk_target_url",
    default=None,
    help="URL the kiosk browser loads at boot.",
)
def gs_display_set(
    resolution: str | None,
    kiosk_enabled: bool | None,
    kiosk_target_url: str | None,
) -> None:
    """Update one or more display fields."""
    body: dict[str, Any] = {}
    if resolution is not None:
        body["resolution"] = resolution
    if kiosk_enabled is not None:
        body["kiosk_enabled"] = kiosk_enabled
    if kiosk_target_url is not None:
        body["kiosk_target_url"] = kiosk_target_url
    if not body:
        click.echo("No fields to update. Use 'ados gs display get' to view.", err=True)
        return
    data = _request("PUT", "/api/v1/ground-station/display", json_body=body)
    if data is not None:
        _pp(data)


# ── gamepad ────────────────────────────────────────────────────────────────


@gs_group.group("gamepad")
def gs_gamepad() -> None:
    """Gamepad list and primary-device selection."""


@gs_gamepad.command("list")
def gs_gamepad_list() -> None:
    """List connected gamepads and the current primary device id."""
    data = _request("GET", "/api/v1/ground-station/gamepads")
    if data is not None:
        _pp(data)


@gs_gamepad.command("primary")
@click.argument("device_id")
def gs_gamepad_primary(device_id: str) -> None:
    """Set the primary gamepad used by the PIC arbiter."""
    data = _request(
        "PUT",
        "/api/v1/ground-station/gamepads/primary",
        json_body={"device_id": device_id},
    )
    if data is not None:
        _pp(data)


# ── pair bt ────────────────────────────────────────────────────────────────


@gs_group.group("pair")
def gs_pair_group() -> None:
    """Pairing helpers (Bluetooth today, expandable later)."""


@gs_pair_group.group("bt")
def gs_pair_bt() -> None:
    """Bluetooth pairing subcommands."""


@gs_pair_bt.command("scan")
@click.option(
    "--duration",
    "duration_s",
    type=int,
    default=10,
    help="Scan duration in seconds (default 10).",
)
def gs_pair_bt_scan(duration_s: int) -> None:
    """Run a BlueZ scan for nearby gamepads."""
    data = _request(
        "POST",
        "/api/v1/ground-station/bluetooth/scan",
        json_body={"duration_s": duration_s},
    )
    if data is not None:
        _pp(data)


@gs_pair_bt.command("pair")
@click.argument("mac")
def gs_pair_bt_pair(mac: str) -> None:
    """Pair with a Bluetooth device by MAC."""
    data = _request(
        "POST",
        "/api/v1/ground-station/bluetooth/pair",
        json_body={"mac": mac},
    )
    if data is not None:
        _pp(data)


@gs_pair_bt.command("forget")
@click.argument("mac")
def gs_pair_bt_forget(mac: str) -> None:
    """Forget a previously-paired Bluetooth device."""
    data = _request(
        "DELETE",
        f"/api/v1/ground-station/bluetooth/{mac}",
    )
    if data is not None:
        _pp(data)


# ── pic ────────────────────────────────────────────────────────────────────


@gs_group.group("pic")
def gs_pic() -> None:
    """Pilot-in-Command arbiter state and control."""


@gs_pic.command("state")
def gs_pic_state() -> None:
    """Print the current PIC state dict."""
    data = _request("GET", "/api/v1/ground-station/pic")
    if data is not None:
        _pp(data)


@gs_pic.command("claim")
@click.argument("client_id")
@click.option("--force", is_flag=True, default=False, help="Force claim without confirm token.")
def gs_pic_claim(client_id: str, force: bool) -> None:
    """Claim PIC for the given client id."""
    body: dict[str, Any] = {"client_id": client_id}
    if force:
        body["force"] = True
    data = _request("POST", "/api/v1/ground-station/pic/claim", json_body=body)
    if data is not None:
        _pp(data)


@gs_pic.command("release")
@click.argument("client_id")
def gs_pic_release(client_id: str) -> None:
    """Release PIC held by the given client id."""
    data = _request(
        "POST",
        "/api/v1/ground-station/pic/release",
        json_body={"client_id": client_id},
    )
    if data is not None:
        _pp(data)


if __name__ == "__main__":
    gs_group()
