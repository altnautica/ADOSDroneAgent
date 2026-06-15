# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Live vision-detection WebSocket bridge.

Streams the vision engine's detection batches to a browser. The engine
re-publishes every ``DetectionBatch`` onto a broadcast Unix socket
(``/run/ados/vision-detections.sock``) as length-prefixed msgpack frames
(4-byte big-endian length + msgpack-named-map body). This route connects
to that socket, decodes each frame, and forwards it to the WebSocket peer
as JSON with the same field names the contract uses:

    {
      "model_id": str,
      "camera_id": str,
      "frame_id": int,
      "ts_ms": int,
      "detections": [
        {
          "bbox": {"x": float, "y": float, "width": float, "height": float},
          "class_label": str,
          "confidence": float,
          "track_id": int | null
        }
      ]
    }

The socket is a last-state broadcast, so a browser that connects after a
detection immediately receives the most recent batch.

Auth: native clients pass ``X-ADOS-Key`` on the handshake; browsers
exchange the pairing key for a one-shot ticket via ``POST /api/_ws/ticket``
with ``scope=vision.detections`` and present it through the
``ados-ws-ticket`` subprotocol. There is no ``?api_key=`` fallback.

CLOUD-RELAY FOLLOW-UP (not built here): a remote drone reached only over
the cloud relay has no LAN path to this socket. The documented follow-up
is a vision/detection MQTT topic published by ``ados-cloud`` (alongside
the existing ``ados/{device_id}/...`` topics) that the GCS subscribes to
for cross-network detections. The browser-side store contract
(``setBatch``) is identical, so wiring that path later is additive.
"""

from __future__ import annotations

import asyncio
import struct

import structlog
from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from ados.core import paths as _paths

log = structlog.get_logger("api.vision_detections")

router = APIRouter()

# The broadcast socket the vision engine binds. Resolved from the same
# runtime root the rest of the agent's sockets use so an ``ADOS_RUN_DIR``
# override (tests, dev rigs) moves it consistently.
VISION_DETECTIONS_SOCK = _paths.ADOS_RUN_DIR / "vision-detections.sock"

# 4-byte big-endian length prefix, matching ados_protocol::frame and the
# state socket. Detection batches are small; the cap is the same generous
# state-frame ceiling (1 MiB) the engine frames against.
_HEADER_SIZE = 4
_MAX_FRAME_SIZE = 1024 * 1024

# WebSocket auth scope. Must match an entry in the control surface's
# ticket-mint allow-set so the GCS can mint a ticket for it.
_WS_SCOPE = "vision.detections"


def _msgpack_loads(data: bytes) -> dict | None:
    """Decode a msgpack detection-batch body, or ``None`` if msgpack is
    unavailable or the frame is malformed.

    ``msgpack`` is an optional dependency on some minimal installs, so the
    import is local and a missing module degrades to "no detections"
    rather than a hard import error at module load.
    """
    try:
        import msgpack  # type: ignore[import-not-found]
    except ImportError:
        log.warning("vision_detections_msgpack_missing")
        return None
    try:
        decoded = msgpack.unpackb(data, raw=False)
    except Exception as exc:  # malformed frame
        log.debug("vision_detections_decode_failed", error=str(exc))
        return None
    return decoded if isinstance(decoded, dict) else None


@router.websocket("/vision/detections/ws")
async def ws_vision_detections(websocket: WebSocket) -> None:
    """Stream live detection batches from the engine to the browser.

    Connects to the engine's ``vision-detections.sock`` broadcast,
    decodes each length-prefixed msgpack ``DetectionBatch``, and forwards
    it to the WebSocket as JSON. The socket's last-state replay means a
    fresh subscriber gets the most recent batch right away.

    The connection is downlink-only (engine → browser). The route does
    not read from the WebSocket; a peer disconnect ends the stream.
    """
    from ados.api.middleware.ws_auth import authenticate_websocket as _ws_auth

    accept_subprotocol = await _ws_auth(websocket, scope=_WS_SCOPE)
    if accept_subprotocol is None:
        return

    if accept_subprotocol:
        await websocket.accept(subprotocol=accept_subprotocol)
    else:
        await websocket.accept()

    sock_path = str(VISION_DETECTIONS_SOCK)
    reader: asyncio.StreamReader | None = None
    writer: asyncio.StreamWriter | None = None
    try:
        # Bounded retry: the engine may still be coming up, or vision may
        # simply not be running on this board. A short retry covers the
        # startup race; a persistent absence closes cleanly so the GCS
        # shows "no live detections" instead of hanging.
        for attempt in range(5):
            try:
                reader, writer = await asyncio.open_unix_connection(sock_path)
                break
            except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
                if attempt < 4:
                    await asyncio.sleep(0.5)
                else:
                    log.info(
                        "vision_detections_sock_unavailable",
                        path=sock_path,
                        error=str(exc),
                    )
                    try:
                        await websocket.close(
                            code=1011, reason="E_VISION_DETECTIONS_UNAVAILABLE"
                        )
                    except RuntimeError:
                        pass
                    return

        assert reader is not None
        while True:
            header = await reader.readexactly(_HEADER_SIZE)
            (length,) = struct.unpack("!I", header)
            if length == 0 or length > _MAX_FRAME_SIZE:
                log.warning("vision_detections_bad_frame_len", length=length)
                break
            body = await reader.readexactly(length)
            batch = _msgpack_loads(body)
            if batch is None:
                continue
            await websocket.send_json(batch)
    except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
        # Engine went away or the socket closed. End the stream.
        pass
    except WebSocketDisconnect:
        pass
    except Exception as exc:  # pragma: no cover - defensive
        log.warning("vision_detections_bridge_error", error=str(exc))
    finally:
        if writer is not None:
            try:
                writer.close()
            except Exception:
                pass
        try:
            await websocket.close()
        except RuntimeError:
            pass
