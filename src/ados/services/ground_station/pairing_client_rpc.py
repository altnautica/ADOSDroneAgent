"""Unix-socket RPC client for the pairing daemon.

REST handlers and any other in-process caller use this to talk to
`ados-mesh-pairing.service` when `ADOS_PAIRING_VIA_DAEMON=1` is set.

Every call is a new connect-send-recv-close cycle. Pairing is
low-frequency (operator pushes a button on the OLED or the GCS) so
amortizing a persistent socket is unnecessary. A fresh connection per
call also sidesteps the "long-lived TCP idle" class of bugs.

Falls back to directly importing `get_pairing_manager()` when the
env flag is not set, so callers can write one set of code:

    from .pairing_client_rpc import pairing_facade
    mgr = pairing_facade()
    await mgr.open_window(duration_s=60)

`pairing_facade()` returns the daemon-backed proxy when the flag is on,
and the in-process PairingManager when it is off.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, Protocol

from ados.core.logging import get_logger

from .pairing_manager import PairingManager, get_pairing_manager

log = get_logger("ground_station.pairing_client_rpc")

SOCKET_PATH = Path("/run/ados/pairing.sock")
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
    except (OSError, asyncio.TimeoutError) as exc:
        raise PairingRpcError(f"pairing daemon unreachable: {exc}") from exc

    try:
        payload = json.dumps({"op": op, "args": args or {}}) + "\n"
        writer.write(payload.encode("utf-8"))
        await writer.drain()
        try:
            raw = await asyncio.wait_for(reader.readline(), timeout=IO_TIMEOUT_S)
        except asyncio.TimeoutError as exc:
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
    """Thin wrapper covering the subset of PairingManager used by REST
    handlers. The daemon builds the invite bundle itself (same disk
    state) so `approve` takes only the device id and returns the
    encoded blob plus issued/expires timestamps in a dict."""

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


class PairingFacade(Protocol):
    """Minimum interface both PairingManager (in-process) and
    PairingDaemonProxy (out-of-process) satisfy for REST usage."""

    async def open_window(self, duration_s: int = ...) -> Any: ...
    async def close_window(self) -> Any: ...
    async def is_window_open(self) -> bool: ...
    async def snapshot(self) -> dict[str, Any]: ...


def use_daemon() -> bool:
    """Check whether the REST path should proxy to the pairing daemon.

    Controlled by `ADOS_PAIRING_VIA_DAEMON=1` in `/etc/ados/env`. Default
    false for backwards compatibility; flipping the flag lets an
    operator opt into the split topology once `ados-mesh-pairing.service`
    is enabled and running.
    """
    return os.environ.get("ADOS_PAIRING_VIA_DAEMON", "").lower() in (
        "1",
        "true",
        "yes",
    )


def pairing_facade() -> PairingManager | PairingDaemonProxy:
    """Return the appropriate pairing handle for this process.

    When the env flag is on, returns a new daemon proxy (cheap; one
    object, no socket held open). When the flag is off, returns the
    in-process singleton — same shape REST handlers have used since
    the daemon split did not exist.
    """
    if use_daemon():
        return PairingDaemonProxy()
    return get_pairing_manager()
