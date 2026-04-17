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


@gs_network.command("show")
def gs_network_show() -> None:
    """Print the full network uplink view (AP, wifi client, ethernet, modem)."""
    data = _request("GET", "/api/v1/ground-station/network")
    if data is not None:
        _pp(data)


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


# ── network client ─────────────────────────────────────────────────────────


@gs_network.group("client")
def gs_network_client() -> None:
    """WiFi client (station) controls."""


@gs_network_client.command("scan")
def gs_network_client_scan() -> None:
    """Scan for nearby WiFi networks."""
    data = _request("GET", "/api/v1/ground-station/network/client/scan")
    if data is not None:
        _pp(data)


@gs_network_client.command("join")
@click.argument("ssid")
@click.option("--passphrase", default=None, help="WPA2 passphrase (optional).")
@click.option("--force", is_flag=True, default=False, help="Steal wlan0 from AP.")
def gs_network_client_join(ssid: str, passphrase: str | None, force: bool) -> None:
    """Join a WiFi network as a station."""
    body: dict[str, Any] = {"ssid": ssid, "force": force}
    if passphrase is not None:
        body["passphrase"] = passphrase
    data = _request("PUT", "/api/v1/ground-station/network/client/join", json_body=body)
    if data is not None:
        _pp(data)


@gs_network_client.command("leave")
def gs_network_client_leave() -> None:
    """Disconnect the current WiFi client connection."""
    data = _request("DELETE", "/api/v1/ground-station/network/client")
    if data is not None:
        _pp(data)


# ── network modem ──────────────────────────────────────────────────────────


@gs_network.group("modem")
def gs_network_modem() -> None:
    """Cellular modem status and configuration."""


@gs_network_modem.command("status")
def gs_network_modem_status() -> None:
    """Print modem status and data usage."""
    data = _request("GET", "/api/v1/ground-station/network/modem")
    if data is not None:
        _pp(data)


@gs_network_modem.command("usage")
def gs_network_modem_usage() -> None:
    """Print modem data usage (alias for status, filters to usage fields)."""
    data = _request("GET", "/api/v1/ground-station/network/modem")
    if data is not None:
        view = {
            "data_used_mb": data.get("data_used_mb"),
            "cap_mb": data.get("cap_mb"),
            "percent": data.get("percent"),
            "iface": data.get("iface"),
        }
        _pp(view)


@gs_network_modem.command("configure")
@click.option("--apn", default=None, help="APN (e.g. 'airtelgprs.com').")
@click.option("--cap-gb", "cap_gb", type=float, default=None, help="Monthly data cap in GB.")
@click.option("--enabled/--disabled", "enabled", default=None, help="Enable or disable the modem.")
def gs_network_modem_configure(
    apn: str | None,
    cap_gb: float | None,
    enabled: bool | None,
) -> None:
    """Update modem config. Any flag omitted is left unchanged."""
    body: dict[str, Any] = {}
    if apn is not None:
        body["apn"] = apn
    if cap_gb is not None:
        body["cap_gb"] = cap_gb
    if enabled is not None:
        body["enabled"] = enabled
    if not body:
        click.echo("No fields to update. Use 'ados gs network modem status' to view.", err=True)
        return
    data = _request("PUT", "/api/v1/ground-station/network/modem", json_body=body)
    if data is not None:
        _pp(data)


# ── network priority ───────────────────────────────────────────────────────


@gs_network.command("priority")
def gs_network_priority() -> None:
    """Print the uplink priority list."""
    data = _request("GET", "/api/v1/ground-station/network/priority")
    if data is not None:
        _pp(data)


@gs_network.command("priority-set")
@click.argument("priority_csv")
def gs_network_priority_set(priority_csv: str) -> None:
    """Set uplink priority. Pass comma-separated names (e.g. 'eth,wifi,modem')."""
    priority = [p.strip() for p in priority_csv.split(",") if p.strip()]
    if not priority:
        click.echo("Error: priority list is empty.", err=True)
        return
    data = _request(
        "PUT",
        "/api/v1/ground-station/network/priority",
        json_body={"priority": priority},
    )
    if data is not None:
        _pp(data)


# ── share-uplink ───────────────────────────────────────────────────────────


@gs_network.command("share-uplink")
@click.argument("state", type=click.Choice(["on", "off"]))
def gs_network_share_uplink(state: str) -> None:
    """Enable or disable IPv4 forwarding + NAT for AP clients."""
    enabled = state == "on"
    data = _request(
        "PUT",
        "/api/v1/ground-station/network/share_uplink",
        json_body={"enabled": enabled},
    )
    if data is not None:
        _pp(data)


# ── DEC-119 / MSN-035: Phase 5 distributed RX (role + mesh) ────────────────


@gs_group.group("role")
def gs_role_group() -> None:
    """Read or change the ground-station mesh role (direct/relay/receiver)."""


@gs_role_group.command("show")
def gs_role_show() -> None:
    data = _request("GET", "/api/v1/ground-station/role")
    if data is not None:
        _pp(data)


@gs_role_group.command("set")
@click.argument("role", type=click.Choice(["direct", "relay", "receiver"]))
def gs_role_set(role: str) -> None:
    """Apply a role transition. Masks/unmasks systemd units in order."""
    data = _request(
        "PUT",
        "/api/v1/ground-station/role",
        json_body={"role": role},
    )
    if data is not None:
        _pp(data)


@gs_group.group("mesh")
def gs_mesh_group() -> None:
    """Inspect and operate the batman-adv local mesh."""


@gs_mesh_group.command("health")
def gs_mesh_health() -> None:
    data = _request("GET", "/api/v1/ground-station/mesh")
    if data is not None:
        _pp(data)


@gs_mesh_group.command("neighbors")
def gs_mesh_neighbors() -> None:
    data = _request("GET", "/api/v1/ground-station/mesh/neighbors")
    if data is None:
        return
    rows = data.get("neighbors", [])
    if not rows:
        click.echo("(no neighbors)")
        return
    click.echo(f"{'mac':<18}  {'iface':<8}  {'tq':>4}  last_seen_ms")
    for n in rows:
        click.echo(
            f"{n.get('mac', '?'):<18}  "
            f"{n.get('iface', '?'):<8}  "
            f"{n.get('tq', 0):>4}  "
            f"{n.get('last_seen_ms', 0)}"
        )


@gs_mesh_group.command("gateways")
def gs_mesh_gateways() -> None:
    data = _request("GET", "/api/v1/ground-station/mesh/gateways")
    if data is None:
        return
    sel = data.get("selected") or "(none)"
    click.echo(f"selected: {sel}")
    rows = data.get("gateways", [])
    if not rows:
        click.echo("(no gateways advertised)")
        return
    click.echo(f"{'sel':<3}  {'mac':<18}  up_kbps   down_kbps  tq")
    for g in rows:
        mark = "*" if g.get("selected") else " "
        click.echo(
            f" {mark}   "
            f"{g.get('mac', '?'):<18}  "
            f"{g.get('class_up_kbps', 0):>7}   "
            f"{g.get('class_down_kbps', 0):>7}    "
            f"{g.get('tq', 0)}"
        )


@gs_mesh_group.command("route")
@click.argument("dest_mac")
def gs_mesh_route(dest_mac: str) -> None:
    """Show the mesh route to a destination MAC."""
    data = _request("GET", "/api/v1/ground-station/mesh/routes")
    if data is None:
        return
    hits = [r for r in data.get("routes", []) if r.get("mac") == dest_mac]
    if not hits:
        click.echo(f"(no route to {dest_mac})")
        return
    for r in hits:
        _pp(r)


@gs_mesh_group.command("accept")
@click.option("--window", "window_s", default=60, show_default=True,
              help="Accept-window duration in seconds (5-300).")
def gs_mesh_accept(window_s: int) -> None:
    """Open the Accept window on a receiver (wait for relay join requests)."""
    data = _request(
        "POST",
        "/api/v1/ground-station/pair/accept",
        json_body={"duration_s": window_s},
    )
    if data is not None:
        _pp(data)


@gs_mesh_group.command("pending")
def gs_mesh_pending() -> None:
    """List relay join requests waiting for approval."""
    data = _request("GET", "/api/v1/ground-station/pair/pending")
    if data is not None:
        _pp(data)


@gs_mesh_group.command("approve")
@click.argument("device_id")
def gs_mesh_approve(device_id: str) -> None:
    """Approve a pending relay by device id."""
    data = _request(
        "POST",
        f"/api/v1/ground-station/pair/approve/{device_id}",
    )
    if data is not None:
        _pp(data)


@gs_mesh_group.command("revoke")
@click.argument("device_id")
def gs_mesh_revoke(device_id: str) -> None:
    """Revoke a previously approved relay."""
    data = _request(
        "POST",
        f"/api/v1/ground-station/pair/revoke/{device_id}",
    )
    if data is not None:
        _pp(data)


@gs_mesh_group.command("join")
@click.option("--receiver-host", default=None, help="Optional receiver hostname hint.")
@click.option("--receiver-port", type=int, default=None, help="Optional receiver UDP port.")
def gs_mesh_join(receiver_host: str | None, receiver_port: int | None) -> None:
    """Relay-side: request to join the current deployment."""
    body: dict[str, Any] = {}
    if receiver_host:
        body["receiver_host"] = receiver_host
    if receiver_port:
        body["receiver_port"] = receiver_port
    data = _request(
        "POST",
        "/api/v1/ground-station/pair/join",
        json_body=body or None,
    )
    if data is not None:
        _pp(data)


if __name__ == "__main__":
    gs_group()
