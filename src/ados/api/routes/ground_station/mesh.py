"""Mesh role + state + gateway-preference endpoints, plus the uplink WS.

Covers:
* GET /role
* GET /mesh, /mesh/neighbors, /mesh/routes, /mesh/gateways
* GET /mesh/config
* WS /ws/uplink
* WS /ws/mesh
"""

from __future__ import annotations

import asyncio
from typing import Any

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
# /ws/uplink
# ---------------------------------------------------------------------------


# How long to sleep between durable-store polls for the uplink WS. The
# router daemon emits net.uplink_active / net.modem_usage at a low rate, so a
# short poll keeps latency low without busy-waiting the store query socket.
_UPLINK_POLL_INTERVAL_S = 1.0


def _uplink_ws_payload(uplink: dict[str, Any], usage: dict[str, Any] | None) -> dict[str, Any]:
    """Shape a stored net.uplink_active body into the uplink WS event payload.

    The data-cap state prefers the live modem-usage block (net.modem_usage),
    falling back to the data_cap_state the uplink event itself carries.
    """
    data_cap_state = None
    if isinstance(usage, dict):
        data_cap_state = usage.get("state")
    if data_cap_state is None:
        data_cap_state = uplink.get("data_cap_state")
    return {
        "kind": "health_changed",
        "active_uplink": uplink.get("active_uplink"),
        "available": uplink.get("available") or [],
        "internet_reachable": bool(uplink.get("internet_reachable")),
        "data_cap_state": data_cap_state,
        "timestamp_ms": uplink.get("timestamp_ms"),
    }


@router.websocket("/ws/uplink")
async def ws_uplink_events(websocket: WebSocket) -> None:
    """Stream uplink-matrix change events as JSON until the client disconnects.

    Mirrors the `/pic/events` pattern: profile-gate before accept so
    wrong-profile callers close with 1008. The uplink health loop runs in
    the native ``ados-net`` daemon, which ships net.uplink_active /
    net.modem_usage to the durable store; this WS polls those events back
    and emits when the snapshot changes (the in-process ``UplinkRouter``
    singleton never ticks in the API process, so its bus is permanently
    silent).

    Native clients pass ``X-ADOS-Key`` on the handshake; browsers
    exchange the pairing key for a one-shot ticket via
    ``POST /api/_ws/ticket`` with ``scope=gs.uplink_events`` and
    present it through the ``ados-ws-ticket`` subprotocol.
    """
    from ados.api.middleware.ws_auth import authenticate_websocket as _ws_auth

    accept_subprotocol = await _ws_auth(websocket, scope="gs.uplink_events")
    if accept_subprotocol is None:
        return

    app = get_agent_app()
    from ados.api.routes.ground_station._common.profile import is_ground_station

    if not is_ground_station(app):
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    if accept_subprotocol:
        await websocket.accept(subprotocol=accept_subprotocol)
    else:
        await websocket.accept()

    from ados.api.sources.network import latest_modem_usage, latest_uplink_active

    last_sent: dict[str, Any] | None = None
    try:
        while True:
            uplink = await latest_uplink_active()
            if uplink is not None:
                usage = await latest_modem_usage()
                payload = _uplink_ws_payload(uplink, usage)
                if payload != last_sent:
                    last_sent = payload
                    await websocket.send_json(payload)
            await asyncio.sleep(_UPLINK_POLL_INTERVAL_S)
    except (WebSocketDisconnect, RuntimeError):
        pass
    except Exception:
        # Store unreachable or socket dropped under us.
        pass


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
