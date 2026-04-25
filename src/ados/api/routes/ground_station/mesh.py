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


# ---------------------------------------------------------------------------
# /role
# ---------------------------------------------------------------------------


@router.get("/role")
async def get_role() -> dict[str, Any]:
    """Read current mesh role plus a capability hint."""
    app = _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import (
        all_mesh_units,
        get_current_role,
        role_units,
    )
    current = get_current_role()
    return {
        "role": current,
        "configured": getattr(
            getattr(app.config, "ground_station", None), "role", "direct"
        ),
        "supported": ["direct", "relay", "receiver"],
        "units": role_units(current),
        "all_mesh_units": all_mesh_units(),
    }


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


@router.get("/mesh")
async def get_mesh_health() -> dict[str, Any]:
    """Snapshot of batman-adv state. 404 with E_NOT_IN_MESH on direct nodes."""
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() == "direct":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_NOT_IN_MESH"}},
        )
    return _gs._read_json_or_empty(_gs._MESH_STATE_JSON)


@router.get("/mesh/neighbors")
async def get_mesh_neighbors() -> dict[str, Any]:
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() == "direct":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_NOT_IN_MESH"}},
        )
    snap = _gs._read_json_or_empty(_gs._MESH_STATE_JSON)
    return {"neighbors": snap.get("neighbors", [])}


@router.get("/mesh/routes")
async def get_mesh_routes() -> dict[str, Any]:
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() == "direct":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_NOT_IN_MESH"}},
        )
    # Routes are derived from neighbors today; mesh_manager can expand
    # this to `batctl o -H` when multi-hop visibility is needed.
    snap = _gs._read_json_or_empty(_gs._MESH_STATE_JSON)
    return {"routes": snap.get("neighbors", [])}


@router.get("/mesh/gateways")
async def get_mesh_gateways() -> dict[str, Any]:
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() == "direct":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_NOT_IN_MESH"}},
        )
    snap = _gs._read_json_or_empty(_gs._MESH_STATE_JSON)
    return {
        "gateways": snap.get("gateways", []),
        "selected": snap.get("selected_gateway"),
    }


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


@router.get("/mesh/config")
async def get_mesh_config() -> dict[str, Any]:
    app = _gs._require_ground_profile()
    mesh = app.config.ground_station.mesh
    return {
        "mesh_id": mesh.mesh_id,
        "carrier": mesh.carrier,
        "channel": mesh.channel,
        "bat_iface": mesh.bat_iface,
        "interface_override": mesh.interface_override,
    }


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


@router.websocket("/ws/uplink")
async def ws_uplink_events(websocket: WebSocket) -> None:
    """Stream UplinkRouter events as JSON until the client disconnects.

    Mirrors the `/pic/events` pattern: profile-gate before accept so
    wrong-profile callers close with 1008; subscribe to the async
    iterator `UplinkEventBus.subscribe()`; JSON-serialize each event.
    """
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    await websocket.accept()

    try:
        from ados.services.ground_station.uplink_router import get_uplink_router
    except Exception:
        await websocket.send_json({"event": "error", "code": "E_UPLINK_ROUTER_UNAVAILABLE"})
        await websocket.close()
        return

    try:
        bus = get_uplink_router().bus
    except Exception:
        await websocket.send_json({"event": "error", "code": "E_UPLINK_BUS_UNAVAILABLE"})
        await websocket.close()
        return

    try:
        async for evt in bus.subscribe():
            try:
                payload = {
                    "kind": evt.kind,
                    "active_uplink": evt.active_uplink,
                    "available": list(evt.available) if evt.available is not None else [],
                    "internet_reachable": bool(evt.internet_reachable),
                    "data_cap_state": evt.data_cap_state,
                    "timestamp_ms": evt.timestamp_ms,
                }
                await websocket.send_json(payload)
            except (WebSocketDisconnect, RuntimeError):
                break
    except WebSocketDisconnect:
        pass
    except Exception:
        # Bus closed or subscriber removed under us.
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
    """
    await websocket.accept()
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.send_json({"event": "error", "code": "E_PROFILE_MISMATCH"})
        await websocket.close()
        return

    from ados.services.ground_station.events import (
        get_mesh_event_bus,
        get_pairing_event_bus,
    )
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
