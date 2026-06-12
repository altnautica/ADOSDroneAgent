"""Client for the native uplink daemon's WiFi-client command socket.

Joining / forgetting an upstream WiFi network is an operator on-demand
action. When the native ``ados-net`` daemon is the running uplink owner, the
REST layer has no in-process Python manager to call (and must NOT drive
``nmcli`` on ``wlan0`` itself, or it would race the daemon's own WiFi-client
manager for the radio). So it forwards each action to the daemon's command
socket at ``/run/ados/wifi-cmd.sock`` instead; the running service applies it
through its single WiFi-client manager (the owner of the wlan0 AP/STA lock).

Wire protocol (same framing as the radio command socket): one
newline-terminated JSON request, one newline-terminated JSON response per
connection, then the server closes.

    {"op": "wifi_join", "ssid": "Net", "passphrase": "pw", "force": false}
        -> {"ok": true, "joined": true, "ip": "...", "gateway": "...", ...}
    {"op": "wifi_forget", "name": "Net"}
        -> {"ok": true, "forgot": true, "name": "Net", "error": null}
    {"op": "wifi_leave"}
        -> {"ok": true, "left": true, "previous_ssid": "Net"}
    {"op": "wifi_status"}
        -> {"ok": true, "connected": true, "ssid": "Net", ...}

A reply with ``ok: false`` carries an ``error`` code; the helpers raise
``NetCmdError`` so the REST layer surfaces it. An unreachable socket (the
native daemon is not up yet, or the box is in the packaged path) raises
``NetCmdUnavailableError`` so the caller can fall back to the packaged manager.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

from ados.core.logging import get_logger
from ados.core.paths import WIFI_CMD_SOCK

log = get_logger("network.wifi_cmd_client")

# Connecting to a live socket is local and near-instant.
_CONNECT_TIMEOUT_S = 2.0
# A join stops hostapd, transitions wlan0 to STA, and waits for an IP, so the
# reply can take a while; the connect itself uses CONNECT_TIMEOUT (30 s) inside
# the daemon, so leave headroom above that.
_REPLY_TIMEOUT_S = 45.0


class NetCmdError(RuntimeError):
    """The command socket reported a failed apply (``ok: false``)."""


class NetCmdUnavailableError(RuntimeError):
    """The command socket could not be reached.

    The native uplink daemon is not serving the socket (it is not up yet, or
    the box is running the packaged uplink path). The caller decides whether to
    fall back to the packaged manager.
    """


async def _roundtrip(req: dict, *, reply_timeout: float = _REPLY_TIMEOUT_S) -> dict:
    """Send one request, read one reply. Raises on an unreachable socket or a
    server-reported failure."""
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(str(WIFI_CMD_SOCK)),
            timeout=_CONNECT_TIMEOUT_S,
        )
    except (TimeoutError, OSError) as exc:
        raise NetCmdUnavailableError(
            f"wifi command socket unavailable at {WIFI_CMD_SOCK}"
        ) from exc

    try:
        writer.write((json.dumps(req) + "\n").encode("utf-8"))
        await writer.drain()
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=reply_timeout)
        except (TimeoutError, OSError) as exc:
            raise NetCmdUnavailableError(
                "wifi command socket did not reply in time"
            ) from exc
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:  # noqa: BLE001 — teardown is best-effort
            pass

    if not line:
        raise NetCmdUnavailableError(
            "wifi command socket closed connection before replying"
        )
    resp = json.loads(line.decode("utf-8"))
    if resp.get("ok") is False:
        raise NetCmdError(resp.get("error") or "unknown wifi command error")
    return resp


def _strip_ok(resp: dict) -> dict[str, Any]:
    """Drop the transport-level ``ok`` flag so the route returns the same shape
    the packaged manager does (joined/forgot/left/status fields only)."""
    return {k: v for k, v in resp.items() if k != "ok"}


async def join(ssid: str, passphrase: str | None, force: bool) -> dict[str, Any]:
    """Join an upstream WiFi network through the native uplink daemon."""
    resp = await _roundtrip(
        {
            "op": "wifi_join",
            "ssid": str(ssid),
            "passphrase": passphrase,
            "force": bool(force),
        }
    )
    return _strip_ok(resp)


async def forget(name: str) -> dict[str, Any]:
    """Delete a saved WiFi profile by name through the native uplink daemon."""
    resp = await _roundtrip({"op": "wifi_forget", "name": str(name)})
    return _strip_ok(resp)


async def leave() -> dict[str, Any]:
    """Disconnect the current WiFi-client link through the native uplink daemon."""
    resp = await _roundtrip({"op": "wifi_leave"})
    return _strip_ok(resp)


async def status() -> dict[str, Any]:
    """Read the WiFi-client station status through the native uplink daemon."""
    resp = await _roundtrip({"op": "wifi_status"}, reply_timeout=10.0)
    return _strip_ok(resp)
