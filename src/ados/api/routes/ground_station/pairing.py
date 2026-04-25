"""Mesh pairing lifecycle endpoints.

Covers:
* POST /pair/accept (open Accept window on receiver)
* POST /pair/close (close Accept window early)
* GET /pair/pending (list pending join requests)
* POST /pair/approve/{device_id} (approve a relay, return invite blob)
* POST /pair/revoke/{device_id} (revoke a previously paired device)
* POST /pair/join (relay sends a join request and waits for invite)
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException

from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station._common import (
    PairAcceptRequest,
    PairJoinRequest,
)
from ados.core.paths import MESH_ID_PATH


router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.post("/pair/accept")
async def post_pair_accept(req: PairAcceptRequest) -> dict[str, Any]:
    """Open the Accept window on a receiver. Idempotent during open window.

    Routes through `pairing_facade()` so when `ADOS_PAIRING_VIA_DAEMON=1`
    the call lands in `ados-mesh-pairing.service` and the UDP bind
    survives a REST restart.
    """
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    from ados.services.ground_station.pairing_client_rpc import (
        PairingRpcError,
        pairing_facade,
        use_daemon,
    )
    mgr = pairing_facade()
    try:
        if use_daemon():
            # Daemon proxy returns a dict, not an AcceptWindow instance.
            result = await mgr.open_window(duration_s=req.duration_s)
            return {
                "opened_at_ms": int(result.get("opened_at_ms", 0)),
                "closes_at_ms": int(result.get("closes_at_ms", 0)),
                "duration_s": req.duration_s,
            }
        window = await mgr.open_window(duration_s=req.duration_s)
        return {
            "opened_at_ms": window.opened_at_ms,
            "closes_at_ms": window.closes_at_ms,
            "duration_s": req.duration_s,
        }
    except PairingRpcError as exc:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_PAIR_DAEMON_UNAVAILABLE", "message": str(exc)}},
        ) from exc


@router.post("/pair/close")
async def post_pair_close() -> dict[str, Any]:
    """Close the receiver Accept window early. Idempotent.

    Called by the OLED when the operator presses B4 during the Accept
    window overlay. Also safe to call when no window is open; returns
    `{"closed": false}` in that case so the caller can distinguish a
    real close from a no-op.
    """
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    from ados.services.ground_station.pairing_client_rpc import (
        PairingRpcError,
        pairing_facade,
        use_daemon,
    )
    mgr = pairing_facade()
    try:
        if use_daemon():
            result = await mgr.close_window()
            return {"closed": bool(result.get("closed", False))}
        was_open = await mgr.is_window_open()
        await mgr.close_window()
        return {"closed": was_open}
    except PairingRpcError as exc:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_PAIR_DAEMON_UNAVAILABLE", "message": str(exc)}},
        ) from exc


@router.get("/pair/pending")
async def get_pair_pending() -> dict[str, Any]:
    _gs._require_ground_profile()
    from ados.services.ground_station.pairing_client_rpc import (
        PairingRpcError,
        pairing_facade,
    )
    try:
        snap = await pairing_facade().snapshot()
        return snap
    except PairingRpcError as exc:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_PAIR_DAEMON_UNAVAILABLE", "message": str(exc)}},
        ) from exc


@router.post("/pair/approve/{device_id}")
async def post_pair_approve(device_id: str) -> dict[str, Any]:
    """Approve a pending relay. Encrypts + returns the invite blob.

    The actual blob transmission is done by the pairing UDP listener
    (either in-process via `ados-api` or out-of-process via
    `ados-mesh-pairing.service` when `ADOS_PAIRING_VIA_DAEMON=1`).
    This handler is a control-plane shortcut for operators using REST
    directly; the field-only OLED flow works without hitting REST.
    """
    app = _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    from ados.services.ground_station.pairing_client_rpc import (
        PairingRpcError,
        use_daemon,
    )

    # Daemon path. The daemon builds its own invite bundle from the
    # same mesh identity files, so we just forward the device id.
    if use_daemon():
        from ados.services.ground_station.pairing_client_rpc import (
            PairingDaemonProxy,
        )
        proxy = PairingDaemonProxy()
        try:
            if not await proxy.is_window_open():
                raise HTTPException(
                    status_code=410,
                    detail={"error": {"code": "E_PAIR_WINDOW_EXPIRED"}},
                )
            result = await proxy.approve(device_id)
        except PairingRpcError as exc:
            msg = str(exc)
            if "not found" in msg or "window closed" in msg:
                raise HTTPException(
                    status_code=404,
                    detail={"error": {"code": "E_PAIR_REQUEST_NOT_FOUND"}},
                ) from exc
            if "mesh not initialized" in msg:
                raise HTTPException(
                    status_code=503,
                    detail={"error": {"code": "E_MESH_NOT_INITIALIZED"}},
                ) from exc
            raise HTTPException(
                status_code=503,
                detail={
                    "error": {
                        "code": "E_PAIR_DAEMON_UNAVAILABLE",
                        "message": msg,
                    }
                },
            ) from exc
        return {
            "device_id": device_id,
            "invite_blob_hex": str(result.get("invite_blob_hex", "")),
            "issued_at_ms": int(result.get("issued_at_ms", 0)),
            "expires_at_ms": int(result.get("expires_at_ms", 0)),
        }

    # In-process path. Build the invite bundle here because the
    # in-process `PairingManager.approve(device_id, bundle)` takes it
    # from the caller rather than reading disk itself.
    from ados.services.ground_station.pairing_manager import (
        InviteBundle,
        get_pairing_manager,
    )
    mgr = get_pairing_manager()
    if not await mgr.is_window_open():
        raise HTTPException(
            status_code=410,
            detail={"error": {"code": "E_PAIR_WINDOW_EXPIRED"}},
        )
    mesh_id_path = MESH_ID_PATH
    psk_path = Path(app.config.ground_station.mesh.shared_key_path)
    try:
        mesh_id = mesh_id_path.read_text(encoding="utf-8").strip()
        psk = psk_path.read_bytes().strip()
    except OSError:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_MESH_NOT_INITIALIZED"}},
        )
    from ados.services.wfb.key_mgr import get_key_paths
    _tx, rx_key_path = get_key_paths()
    try:
        wfb_rx_key = Path(rx_key_path).read_bytes()
    except OSError:
        wfb_rx_key = b""
    import socket as _sock
    hostname = _sock.gethostname()
    now_ms = int(time.time() * 1000)
    bundle = InviteBundle(
        mesh_id=mesh_id,
        mesh_psk=psk,
        drone_channel=app.config.video.wfb.channel,
        wfb_rx_key=wfb_rx_key,
        receiver_mdns_host=hostname,
        receiver_mdns_port=5800,
        issued_at_ms=now_ms,
        expires_at_ms=now_ms + 120_000,
    )
    blob = await mgr.approve(device_id, bundle)
    if blob is None:
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_PAIR_REQUEST_NOT_FOUND"}},
        )
    return {
        "device_id": device_id,
        "invite_blob_hex": blob.hex(),
        "issued_at_ms": bundle.issued_at_ms,
        "expires_at_ms": bundle.expires_at_ms,
    }


@router.post("/pair/revoke/{device_id}")
async def post_pair_revoke(device_id: str) -> dict[str, Any]:
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=409,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    from ados.services.ground_station.pairing_manager import revoke as _revoke
    _revoke(device_id)
    return {"device_id": device_id, "revoked": True}


@router.post("/pair/join")
async def post_pair_join(req: PairJoinRequest) -> dict[str, Any]:
    """Relay-side: send a join request and wait for the encrypted invite.

    Synchronously runs the ECDH exchange, decrypts the invite, and
    persists mesh identity to disk. Returns success so the caller can
    promote the node to `relay` and start mesh services.
    """
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    current = get_current_role()
    if current == "receiver":
        raise HTTPException(
            status_code=409,
            detail={
                "error": {
                    "code": "E_WRONG_ROLE",
                    "required": "direct_or_relay",
                    "current": current,
                }
            },
        )
    from ados.services.ground_station.pairing_client import request_join
    result = await request_join(
        receiver_host=req.receiver_host,
        receiver_port=req.receiver_port,
    )
    if not result.ok:
        raise HTTPException(
            status_code=503,
            detail={
                "error": {
                    "code": result.error_code or "E_JOIN_FAILED",
                    "message": result.error_message or "join failed",
                }
            },
        )
    return {
        "mesh_id": result.mesh_id,
        "receiver_host": result.receiver_host,
        "ok": True,
    }
