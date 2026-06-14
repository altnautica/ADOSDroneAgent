"""Mesh role + state + gateway-preference endpoints, plus the uplink WS.

Covers:
* GET/PUT /role
* GET /mesh, /mesh/neighbors, /mesh/routes, /mesh/gateways
* PUT /mesh/gateway_preference
* GET/PUT /mesh/config
* WS /ws/uplink
* WS /ws/mesh
"""

from __future__ import annotations

import asyncio
from typing import Any

from fastapi import APIRouter, HTTPException, WebSocket, WebSocketDisconnect

from ados.api.deps import get_agent_app
from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station._common import (
    MeshConfigUpdate,
    MeshGatewayPreferenceUpdate,
    RoleChangeRequest,
)
from ados.core.paths import MESH_GATEWAY_JSON, PROFILE_CONF

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

    _mesh_event_tailer_task = asyncio.create_task(
        tail_mesh_events(), name="mesh-event-tailer"
    )


# ---------------------------------------------------------------------------
# /role
# ---------------------------------------------------------------------------


@router.put("/role")
async def put_role(req: RoleChangeRequest) -> dict[str, Any]:
    """Change mesh role. Applies mask/unmask + start/stop in order."""
    app = _gs._require_ground_profile()
    # Mesh capability gate: profile.conf is YAML, written by the
    # ground-station install path and refreshed by profile_detect when
    # it sees a second USB WiFi adapter. Nodes without the flag cannot
    # assume a mesh role; direct remains allowed so an opt-out path is
    # available even if the flag is missing.
    profile_conf = _gs._read_yaml_or_empty(PROFILE_CONF)
    mesh_capable = bool(profile_conf.get("mesh_capable", False))
    if req.role in ("relay", "receiver") and not mesh_capable:
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_MESH_NOT_CAPABLE"}},
        )

    # Paired-identity gate for relay: transitioning a fresh box into the
    # relay role with no mesh_id or psk on disk would send mesh_manager
    # into a restart loop. Force the operator to pair first. `direct`
    # and `receiver` have no such requirement (receiver generates its
    # own identity on first boot of mesh_manager).
    if req.role == "relay":
        try:
            from ados.services.ground_station.pairing_client import (
                has_persisted_identity,
            )
            paired = has_persisted_identity()
        except Exception:
            paired = False
        if not paired:
            raise HTTPException(
                status_code=409,
                detail={
                    "error": {
                        "code": "E_NOT_PAIRED",
                        "message": "relay role requires a completed pair with a receiver",
                    }
                },
            )

    from ados.services.ground_station.role_manager import apply_role
    try:
        result = await apply_role(req.role, reason="rest")
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_INVALID_ROLE", "message": str(exc)}},
        )
    # Persist to config so the value survives a reboot even if the
    # sentinel file is wiped.
    try:
        app.config.ground_station.role = req.role
        _gs._save_config(app)
    except Exception:
        pass
    return result


# ---------------------------------------------------------------------------
# /mesh
# ---------------------------------------------------------------------------


@router.put("/mesh/gateway_preference")
async def put_gateway_preference(
    update: MeshGatewayPreferenceUpdate,
) -> dict[str, Any]:
    """Pin a gateway, let batman auto-pick, or disable client mode.

    Persists the preference to `/etc/ados/mesh/gateway.json` so a pin
    survives agent and mesh restarts. `mesh_manager` re-applies the
    pin at setup time. Direct exec also happens here as a convenience
    so operators see immediate feedback without waiting for a restart.
    """
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() == "direct":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_NOT_IN_MESH"}},
        )
    import json as _json
    import os as _os
    import subprocess as _sp
    # Persist first. If the write fails we still apply (some kernels have
    # /etc read-only briefly on upgrade) but surface the error in the
    # response so the UI knows the pin will not survive restart.
    gateway_path = MESH_GATEWAY_JSON
    persist_error: str | None = None
    try:
        gateway_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = gateway_path.with_suffix(gateway_path.suffix + ".tmp")
        tmp.write_text(
            _json.dumps(
                {
                    "mode": update.mode,
                    "pinned_mac": update.pinned_mac,
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
        _os.replace(str(tmp), str(gateway_path))
    except OSError as exc:
        persist_error = str(exc)

    try:
        if update.mode == "off":
            _sp.run(["batctl", "gw_mode", "off"], check=False, timeout=5)
        else:
            _sp.run(["batctl", "gw_mode", "client"], check=False, timeout=5)
            if update.mode == "pinned" and update.pinned_mac:
                _sp.run(
                    ["batctl", "gw_sel", update.pinned_mac],
                    check=False,
                    timeout=5,
                )
    except FileNotFoundError:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_BATCTL_UNAVAILABLE"}},
        )
    resp: dict[str, Any] = {
        "mode": update.mode,
        "pinned_mac": update.pinned_mac,
        "persisted": persist_error is None,
    }
    if persist_error is not None:
        resp["persist_error"] = persist_error
    return resp


@router.put("/mesh/config")
async def put_mesh_config(update: MeshConfigUpdate) -> dict[str, Any]:
    app = _gs._require_ground_profile()
    changed = False
    mesh = app.config.ground_station.mesh
    if update.mesh_id is not None:
        mesh.mesh_id = update.mesh_id
        changed = True
    if update.carrier is not None:
        mesh.carrier = update.carrier
        changed = True
    if update.channel is not None:
        mesh.channel = update.channel
        changed = True
    if changed:
        _gs._save_config(app)
    return {
        "mesh_id": mesh.mesh_id,
        "carrier": mesh.carrier,
        "channel": mesh.channel,
        "applied": changed,
    }


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
