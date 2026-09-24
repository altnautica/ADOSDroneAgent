"""Unix-socket RPC client for the pairing daemon.

The mesh pairing state (the Accept window, the pending join list, the UDP
listener) lives in exactly one process: ``ados-mesh-pairing.service``. REST
handlers reach it through :class:`PairingDaemonProxy`, and so does the native
``GET /pair/pending`` route, so both surfaces read the same window. There is
no in-process fallback: a second owner is how an Accept window opened from the
GCS came to be invisible to the pending-requests view.

Every call is a new connect-send-recv-close cycle. Pairing is low-frequency
(an operator pushes a button on the OLED or the GCS), so a persistent socket
buys nothing, and a fresh connection sidesteps idle-connection failures.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

from ados.core.logging import get_logger
from ados.core.paths import PAIRING_SOCK

log = get_logger("ground_station.pairing_client_rpc")

SOCKET_PATH = PAIRING_SOCK
CONNECT_TIMEOUT_S = 2.0
IO_TIMEOUT_S = 5.0


class PairingRpcError(RuntimeError):
    """Raised when the daemon returns `{"ok": false, ...}` or the
    socket round-trip fails."""


async def _call(op: str, args: dict[str, Any] | None = None) -> dict[str, Any]:
    """Single-shot Unix socket RPC. Raises PairingRpcError on failure."""
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(str(SOCKET_PATH)),
            timeout=CONNECT_TIMEOUT_S,
        )
    except (TimeoutError, OSError) as exc:
        raise PairingRpcError(f"pairing daemon unreachable: {exc}") from exc

    try:
        payload = json.dumps({"op": op, "args": args or {}}) + "\n"
        writer.write(payload.encode("utf-8"))
        await writer.drain()
        try:
            raw = await asyncio.wait_for(reader.readline(), timeout=IO_TIMEOUT_S)
        except TimeoutError as exc:
            raise PairingRpcError("pairing daemon read timeout") from exc
        if not raw:
            raise PairingRpcError("pairing daemon closed connection early")
        reply = json.loads(raw.decode("utf-8"))
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass

    if not reply.get("ok"):
        raise PairingRpcError(str(reply.get("error") or "unknown error"))
    return reply.get("result") or {}


class PairingDaemonProxy:
    """The REST handlers' handle on the pairing daemon.

    The daemon builds the invite bundle itself from the mesh identity on
    disk, so ``approve`` takes only the device id and returns the encoded
    blob plus its issued/expires timestamps.
    """

    async def open_window(self, duration_s: int = 60) -> dict[str, Any]:
        return await _call("open_window", {"duration_s": duration_s})

    async def close_window(self) -> dict[str, Any]:
        return await _call("close_window", {})

    async def is_window_open(self) -> bool:
        result = await _call("is_window_open", {})
        return bool(result.get("open"))

    async def snapshot(self) -> dict[str, Any]:
        return await _call("snapshot", {})

    async def approve(self, device_id: str) -> dict[str, Any]:
        """Approve a pending relay. Returns the full result dict with
        `approved`, `invite_blob_hex`, `issued_at_ms`, `expires_at_ms`.
        Raises `PairingRpcError` if the daemon rejects the approval."""
        return await _call("approve", {"device_id": device_id})

    async def revoke(self, device_id: str) -> bool:
        result = await _call("revoke", {"device_id": device_id})
        return bool(result.get("revoked"))
