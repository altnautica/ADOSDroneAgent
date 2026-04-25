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
import socket
import sys
import time
from pathlib import Path
from typing import Any

import structlog

from ados.core.logging import configure_logging, get_logger
from ados.core.paths import MESH_ID_PATH, PAIRING_SOCK

from .pairing_manager import (
    InviteBundle,
    get_pairing_manager,
    revoke as revoke_device,
)

log = get_logger("ground_station.pairing_daemon")

SOCKET_PATH = PAIRING_SOCK


def _build_invite_bundle() -> InviteBundle | None:
    """Assemble an InviteBundle from the local mesh + wfb state.

    Reads `/etc/ados/mesh/id`, the configured mesh PSK, the drone WFB
    rx key, and the receiver's own hostname. Returns None if the mesh
    is not yet initialized. The same fields that the REST handler used
    to collect at call time are now read here so the daemon can approve
    a relay without the REST process needing to pass the bundle over
    the socket.
    """
    from ados.core.config import load_config
    from ados.services.wfb.key_mgr import get_key_paths

    config = load_config()
    mesh_id_path = MESH_ID_PATH
    psk_path = Path(config.ground_station.mesh.shared_key_path)
    try:
        mesh_id = mesh_id_path.read_text(encoding="utf-8").strip()
        psk = psk_path.read_bytes().strip()
    except OSError:
        return None

    _tx, rx_key_path = get_key_paths()
    try:
        wfb_rx_key = Path(rx_key_path).read_bytes()
    except OSError:
        wfb_rx_key = b""

    hostname = socket.gethostname()
    now_ms = int(time.time() * 1000)
    return InviteBundle(
        mesh_id=mesh_id,
        mesh_psk=psk,
        drone_channel=config.video.wfb.channel,
        wfb_rx_key=wfb_rx_key,
        receiver_mdns_host=hostname,
        receiver_mdns_port=5800,
        issued_at_ms=now_ms,
        expires_at_ms=now_ms + 120_000,
    )


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
            was_open = await mgr.is_window_open()
            await mgr.close_window()
            return {"ok": True, "result": {"closed": was_open}}
        if op == "is_window_open":
            return {"ok": True, "result": {"open": await mgr.is_window_open()}}
        if op == "snapshot":
            return {"ok": True, "result": await mgr.snapshot()}
        if op == "approve":
            device_id = str(args.get("device_id", ""))
            if not device_id:
                return {"ok": False, "error": "device_id required"}
            bundle = _build_invite_bundle()
            if bundle is None:
                return {"ok": False, "error": "mesh not initialized"}
            blob = await mgr.approve(device_id, bundle)
            if blob is None:
                return {"ok": False, "error": "pair request not found or window closed"}
            return {
                "ok": True,
                "result": {
                    "approved": True,
                    "invite_blob_hex": blob.hex(),
                    "issued_at_ms": bundle.issued_at_ms,
                    "expires_at_ms": bundle.expires_at_ms,
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
    # Root-only. The daemon and its sole in-process client (the REST
    # handler in ados-api.service) both run as root, so 0o600 is
    # sufficient and prevents an unprivileged local user from sending
    # arbitrary approve/revoke RPCs that would admit or eject a relay
    # without operator consent.
    try:
        os.chmod(str(SOCKET_PATH), 0o600)
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
