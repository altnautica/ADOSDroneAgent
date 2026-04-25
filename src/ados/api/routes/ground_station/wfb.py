"""WFB radio config, pair-key install, and distributed receive endpoints.

Covers:
* GET/PUT /wfb (channel, bitrate profile, FEC)
* POST/DELETE /wfb/pair (drone pair-key install and unpair)
* GET /wfb/relay/status, /wfb/receiver/relays, /wfb/receiver/combined
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException

from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station._common import (
    PairRequest,
    WfbUpdate,
)


router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.get("/wfb")
async def get_ground_station_wfb() -> dict[str, Any]:
    """Current radio config as stored in agent config."""
    app = _gs._require_ground_profile()
    return _gs._read_wfb_view(app)


@router.put("/wfb")
async def put_ground_station_wfb(update: WfbUpdate) -> dict[str, Any]:
    """Update channel, bitrate profile, or FEC and persist."""
    app = _gs._require_ground_profile()

    wfb_cfg = getattr(app.config, "wfb", None)
    if wfb_cfg is None:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_WFB_CONFIG_MISSING"}},
        )

    if update.channel is not None and hasattr(wfb_cfg, "channel"):
        setattr(wfb_cfg, "channel", update.channel)
    if update.bitrate_profile is not None and hasattr(wfb_cfg, "bitrate_profile"):
        setattr(wfb_cfg, "bitrate_profile", update.bitrate_profile)
    if update.fec is not None and hasattr(wfb_cfg, "fec"):
        setattr(wfb_cfg, "fec", update.fec)

    _gs._save_config(app)
    return _gs._read_wfb_view(app)


@router.post("/wfb/pair")
async def post_wfb_pair(req: PairRequest) -> dict[str, Any]:
    """Install a drone pair key. 409 if already paired."""
    _gs._require_ground_profile()

    pm = _gs._pair_manager()

    try:
        current = await pm.status()
    except Exception:
        current = {"paired": False}

    if current.get("paired"):
        raise HTTPException(
            status_code=409,
            detail={
                "error": {
                    "code": "E_ALREADY_PAIRED",
                    "message": "unpair before pairing a new drone",
                    "paired_drone_id": current.get("paired_drone_id"),
                }
            },
        )

    try:
        result = await pm.pair(
            pair_key=req.pair_key,
            drone_device_id=req.drone_device_id,
        )
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_INVALID_PAIR_KEY", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_FAILED", "message": str(exc)}},
        ) from exc

    return result


@router.delete("/wfb/pair")
async def delete_wfb_pair() -> dict[str, Any]:
    """Remove the installed pair key."""
    _gs._require_ground_profile()

    pm = _gs._pair_manager()
    try:
        return await pm.unpair()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UNPAIR_FAILED", "message": str(exc)}},
        ) from exc


@router.get("/wfb/relay/status")
async def get_wfb_relay_status() -> dict[str, Any]:
    """Relay-side WFB fragment counters + receiver reachability."""
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "relay":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "relay"}},
        )
    return _gs._read_json_or_empty(_gs._WFB_RELAY_JSON)


@router.get("/wfb/receiver/relays")
async def get_wfb_receiver_relays() -> dict[str, Any]:
    """Per-relay fragment counters on the receiver side."""
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    snap = _gs._read_json_or_empty(_gs._WFB_RECEIVER_JSON)
    return {"relays": snap.get("relays", [])}


@router.get("/wfb/receiver/combined")
async def get_wfb_receiver_combined() -> dict[str, Any]:
    """Receiver's combined FEC output stats + stream bitrate."""
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "receiver":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "receiver"}},
        )
    snap = _gs._read_json_or_empty(_gs._WFB_RECEIVER_JSON)
    return {
        "fragments_after_dedup": snap.get("fragments_after_dedup", 0),
        "fec_repaired": snap.get("fec_repaired", 0),
        "output_kbps": snap.get("output_kbps", 0),
        "up": snap.get("up", False),
    }
