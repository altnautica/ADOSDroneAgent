"""Out-of-process pairing daemon.

Today `PairingManager` lives inside `ados-api.service` as a module-level
singleton. The UDP 5801 listener is bound by that process. An agent
restart (upgrades, crash, code reload) tears down the listener, so any
relay that sends a join request during the restart window gets no
response.

This daemon factors the UDP-owning half of the pairing lifecycle into
its own `ados-mesh-pairing.service` systemd unit. REST routes and OLED
can either:
  (a) keep calling `get_pairing_manager()` in-process (current default),
      in which case this daemon stays stopped; or
  (b) set `ADOS_PAIRING_VIA_DAEMON=1` in `/etc/ados/env` so the REST
      process proxies to this daemon over a Unix socket
      (`/run/ados/pairing.sock`). In that mode the UDP bind survives
      REST restarts.

The wire protocol on the Unix socket is deliberately tiny:

    > {"op": "open_window", "args": {"duration_s": 60}}
    < {"ok": true, "result": {...}}

    > {"op": "approve", "args": {"device_id": "..."}}
    < {"ok": false, "error": "not found"}

One request per line. JSON in, JSON out. No framing, no version
negotiation. Clients close the socket after each request; reconnects
are free because pairing is low-frequency.

Supported ops:
  - `open_window(duration_s: int)` -> `{opened_at_ms, closes_at_ms}`
  - `close_window()` -> `{closed: bool}`
  - `is_window_open()` -> `{open: bool}`
  - `snapshot()` -> `{...}` (same shape as REST /pair/pending)
  - `approve(device_id: str)` -> `{approved: bool, ...}`
  - `revoke(device_id: str)` -> `{revoked: bool}`

Every op is routed to the single in-process `get_pairing_manager()`
singleton so state (key pair, pending list, revocations) remains
consistent whichever path the caller used.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
from pathlib import Path
from typing import Any

import structlog

from ados.core.logging import configure_logging, get_logger

from .pairing_manager import get_pairing_manager, revoke as revoke_device

log = get_logger("ground_station.pairing_daemon")

SOCKET_PATH = Path("/run/ados/pairing.sock")


async def _handle_op(op: str, args: dict[str, Any]) -> dict[str, Any]:
    """Dispatch a single RPC op to the in-process PairingManager."""
    mgr = get_pairing_manager()
    try:
        if op == "open_window":
            duration_s = int(args.get("duration_s", 60))
            window = await mgr.open_window(duration_s=duration_s)
            return {
                "ok": True,
                "result": {
                    "opened_at_ms": window.opened_at_ms,
                    "closes_at_ms": window.closes_at_ms,
                },
            }
        if op == "close_window":
            await mgr.close_window()
            return {"ok": True, "result": {"closed": True}}
        if op == "is_window_open":
            return {"ok": True, "result": {"open": await mgr.is_window_open()}}
        if op == "snapshot":
            return {"ok": True, "result": await mgr.snapshot()}
        if op == "approve":
            device_id = str(args.get("device_id", ""))
            if not device_id:
                return {"ok": False, "error": "device_id required"}
            blob = await mgr.approve(device_id)
            return {
                "ok": blob is not None,
                "result": {
                    "approved": blob is not None,
                    "invite_bytes": len(blob) if blob else 0,
                },
            }
        if op == "revoke":
            device_id = str(args.get("device_id", ""))
            if not device_id:
                return {"ok": False, "error": "device_id required"}
            revoke_device(device_id)
            return {"ok": True, "result": {"revoked": True}}
        return {"ok": False, "error": f"unknown op: {op}"}
    except Exception as exc:  # noqa: BLE001 — any failure returns as error
        log.exception("pairing_daemon_op_failed", op=op)
        return {"ok": False, "error": str(exc)}


async def _serve_connection(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
) -> None:
    """One request per connection. Close after replying."""
    try:
        raw = await asyncio.wait_for(reader.readline(), timeout=5.0)
    except asyncio.TimeoutError:
        writer.close()
        await writer.wait_closed()
        return
    if not raw:
        writer.close()
        await writer.wait_closed()
        return
    try:
        req = json.loads(raw.decode("utf-8"))
        op = str(req.get("op", ""))
        args = req.get("args") or {}
        if not isinstance(args, dict):
            raise ValueError("args must be object")
    except (ValueError, TypeError) as exc:
        reply = {"ok": False, "error": f"malformed request: {exc}"}
    else:
        reply = await _handle_op(op, args)
    try:
        writer.write((json.dumps(reply) + "\n").encode("utf-8"))
        await writer.drain()
    except Exception as exc:
        log.debug("pairing_daemon_reply_failed", error=str(exc))
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass


async def main() -> None:
    from ados.core.config import load_config

    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("pairing_daemon_starting", socket=str(SOCKET_PATH))

    # Ensure parent dir and remove stale socket from a previous run. An
    # orphaned socket inode will refuse `bind` with "Address already in
    # use" on Linux, same as any AF_UNIX listener.
    SOCKET_PATH.parent.mkdir(parents=True, exist_ok=True)
    if SOCKET_PATH.exists():
        try:
            SOCKET_PATH.unlink()
        except OSError as exc:
            slog.error("pairing_daemon_stale_socket_cleanup_failed", error=str(exc))
            sys.exit(2)

    # Pre-touch the UDP listener by opening the window briefly? No: the
    # daemon only binds UDP 5801 when a window is actually open. That
    # matches the in-process behavior and avoids holding the port when
    # no operator has asked for pairing.
    server = await asyncio.start_unix_server(_serve_connection, path=str(SOCKET_PATH))
    # World read/write so the REST and OLED service users can speak to
    # the daemon. File-system permissions are the access control; the
    # socket itself does not authenticate.
    try:
        os.chmod(str(SOCKET_PATH), 0o666)
    except OSError as exc:
        slog.warning("pairing_daemon_socket_chmod_failed", error=str(exc))

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, shutdown.set)
        except (NotImplementedError, RuntimeError):
            pass

    async with server:
        slog.info("pairing_daemon_ready")
        await shutdown.wait()

    slog.info("pairing_daemon_stopping")
    try:
        SOCKET_PATH.unlink()
    except OSError:
        pass


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
