"""Mesh + pairing event WebSocket fan-out for the ground-station profile.

Covers:
* WS /ws/mesh

The uplink change stream (`/ws/uplink`) is served natively by the front and is
no longer registered here.
"""

from __future__ import annotations

import asyncio

from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])

# Process-wide singleton for the cross-process mesh-event tailer. When the
# relay/receiver loops run as their own process (the native data-plane binary),
# they append events to the mesh-event journal instead of the in-process bus.
# This tailer follows that journal and republishes each event onto the bus the
# WebSocket below subscribes to, so a native-relay node lights up the GCS mesh
# events exactly like a same-process manager would. Started lazily on the first
# /ws/mesh connection so it only runs on the ground-station profile.
_mesh_event_tailer_task: asyncio.Task | None = None


def _ensure_mesh_event_tailer() -> None:
    """Start the cross-process mesh-event tailer once per API process."""
    global _mesh_event_tailer_task
    if _mesh_event_tailer_task is not None and not _mesh_event_tailer_task.done():
        return
    from ados.services.ground_station.mesh_event_tailer import tail_mesh_events

    _mesh_event_tailer_task = asyncio.create_task(tail_mesh_events(), name="mesh-event-tailer")


# ---------------------------------------------------------------------------
# /ws/mesh
# ---------------------------------------------------------------------------


@router.websocket("/ws/mesh")
async def ws_mesh_events(websocket: WebSocket) -> None:
    """Stream mesh + pairing events to the GCS.

    Gated like all other ground-station endpoints: closes on drone
    profile. Fans `MeshEvent` and `PairingEvent` into the same socket
    so GCS only needs one subscription for the Hardware tab.

    Native clients pass ``X-ADOS-Key`` on the handshake; browsers
    exchange the pairing key for a one-shot ticket via
    ``POST /api/_ws/ticket`` with ``scope=gs.mesh_events`` and present
    it through the ``ados-ws-ticket`` subprotocol.
    """
    from ados.api.middleware.ws_auth import authenticate_websocket as _ws_auth

    accept_subprotocol = await _ws_auth(websocket, scope="gs.mesh_events")
    if accept_subprotocol is None:
        return

    app = get_agent_app()
    from ados.api.routes.ground_station._common.profile import is_ground_station

    if not is_ground_station(app):
        # Profile-mismatch path: accept briefly so the JSON error reaches
        # the client, then close. Matches the prior behaviour.
        if accept_subprotocol:
            await websocket.accept(subprotocol=accept_subprotocol)
        else:
            await websocket.accept()
        await websocket.send_json({"event": "error", "code": "E_PROFILE_MISMATCH"})
        await websocket.close()
        return

    if accept_subprotocol:
        await websocket.accept(subprotocol=accept_subprotocol)
    else:
        await websocket.accept()

    from ados.services.ground_station.events import (
        get_mesh_event_bus,
        get_pairing_event_bus,
    )

    # Bridge the cross-process mesh-event journal onto the in-process bus so a
    # native relay/receiver process's events reach this WebSocket.
    _ensure_mesh_event_tailer()
    mesh_bus = get_mesh_event_bus()
    pair_bus = get_pairing_event_bus()

    async def _forward_mesh() -> None:
        try:
            async for evt in mesh_bus.subscribe():
                try:
                    await websocket.send_json(
                        {
                            "bus": "mesh",
                            "kind": evt.kind,
                            "timestamp_ms": evt.timestamp_ms,
                            "payload": evt.payload,
                        }
                    )
                except (WebSocketDisconnect, RuntimeError):
                    return
        except Exception:
            return

    async def _forward_pair() -> None:
        try:
            async for evt in pair_bus.subscribe():
                try:
                    await websocket.send_json(
                        {
                            "bus": "pair",
                            "kind": evt.kind,
                            "timestamp_ms": evt.timestamp_ms,
                            "payload": evt.payload,
                        }
                    )
                except (WebSocketDisconnect, RuntimeError):
                    return
        except Exception:
            return

    mesh_task = asyncio.create_task(_forward_mesh())
    pair_task = asyncio.create_task(_forward_pair())
    try:
        done, _pending = await asyncio.wait(
            [mesh_task, pair_task],
            return_when=asyncio.FIRST_COMPLETED,
        )
    except WebSocketDisconnect:
        pass
    finally:
        for t in (mesh_task, pair_task):
            if not t.done():
                t.cancel()
        await asyncio.gather(mesh_task, pair_task, return_exceptions=True)
