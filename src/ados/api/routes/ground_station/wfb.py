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
    """Update channel, bitrate profile, or FEC and persist.

    Returns the radio view plus a `persisted` flag so the operator
    sees clearly when an in-memory mutation did not survive to disk.
    """
    app = _gs._require_ground_profile()

    # WfbConfig lives at app.config.video.wfb, not app.config.wfb. The
    # earlier lookup at the root always returned None and the handler
    # raised E_WFB_CONFIG_MISSING on every call.
    video_cfg = getattr(app.config, "video", None)
    wfb_cfg = getattr(video_cfg, "wfb", None) if video_cfg is not None else None
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

    # Persist via the runtime's save_config helper. Previously this
    # branch inlined the load-modify-save dance against the raw YAML
    # dict; that's now centralized on the runtime so flock + euid
    # checks apply uniformly across every PUT surface.
    persisted = False
    persist_error: str | None = None
    try:
        persisted = bool(app.save_config())
    except Exception as exc:  # noqa: BLE001
        persist_error = str(exc)
        from structlog import get_logger
        get_logger().warning(
            "wfb_config_persist_failed",
            channel=update.channel,
            bitrate_profile=update.bitrate_profile,
            fec=update.fec,
            error=persist_error,
        )

    view = dict(_gs._read_wfb_view(app))
    view["persisted"] = persisted
    if persist_error is not None:
        view["persist_error"] = persist_error
    return view


@router.post("/wfb/pair")
async def post_wfb_pair(req: PairRequest) -> dict[str, Any]:
    """Install a 64-byte rx-side wfb-ng key on the GS.

    Used by the cloud-relay path: the orchestrator running in the GCS
    forwards a base64-encoded blob produced by `wfb_keygen` on the
    paired drone (or by the GS itself when it is the keypair authority
    and is shipping the matching key remotely).

    For the local-bind protocol, callers should hit
    `POST /api/wfb/pair/local-bind` instead.
    """
    import base64

    _gs._require_ground_profile()

    if req.pair_key and not req.blob_b64:
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_PAIR_KEY_DEPRECATED",
                    "message": (
                        "typed pair_key is no longer supported; pass blob_b64 "
                        "(base64 of 64-byte wfb-ng key) or use POST "
                        "/api/wfb/pair/local-bind"
                    ),
                }
            },
        )
    if not req.blob_b64:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_BLOB_REQUIRED"}},
        )

    pm = _gs._pair_manager()

    try:
        current = await pm.status("gs")
    except Exception:
        current = {"paired": False}

    if current.get("paired"):
        raise HTTPException(
            status_code=409,
            detail={
                "error": {
                    "code": "E_ALREADY_PAIRED",
                    "message": "unpair before pairing a new drone",
                    "paired_with_device_id": current.get("paired_with_device_id"),
                }
            },
        )

    try:
        blob = base64.b64decode(req.blob_b64, validate=True)
    except (ValueError, TypeError) as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_BLOB_BASE64", "message": str(exc)}},
        ) from exc

    try:
        return await pm.apply_keypair(blob, "gs", req.drone_device_id)
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_INVALID_KEY_BLOB", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_FAILED", "message": str(exc)}},
        ) from exc


@router.delete("/wfb/pair")
async def delete_wfb_pair() -> dict[str, Any]:
    """Remove the installed pair key on the GS side."""
    _gs._require_ground_profile()

    pm = _gs._pair_manager()
    try:
        return await pm.unpair("gs")
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UNPAIR_FAILED", "message": str(exc)}},
        ) from exc


@router.get("/wfb/relay/status")
async def get_wfb_relay_status() -> dict[str, Any]:
    """Relay-side WFB fragment counters + receiver reachability.

    Reads the durable store's most-recent relay state first (the relay loop
    ships the same body it writes to the sidecar), falling back to the sidecar
    file when the store is unreachable, so a losable store degrades to the old
    behavior, never to a 500.
    """
    _gs._require_ground_profile()
    from ados.services.ground_station.role_manager import get_current_role
    if get_current_role() != "relay":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_WRONG_ROLE", "required": "relay"}},
        )
    from ados.api.sources.gs import latest_relay_state
    detail = await latest_relay_state()
    if detail is not None:
        return detail
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
    from ados.api.sources.gs import latest_receiver_state, slice_receiver_relays
    detail = await latest_receiver_state()
    if detail is not None:
        return slice_receiver_relays(detail)
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
    from ados.api.sources.gs import latest_receiver_state, slice_receiver_combined
    detail = await latest_receiver_state()
    if detail is not None:
        return slice_receiver_combined(detail)
    snap = _gs._read_json_or_empty(_gs._WFB_RECEIVER_JSON)
    return {
        "fragments_after_dedup": snap.get("fragments_after_dedup", 0),
        "fec_repaired": snap.get("fec_repaired", 0),
        "output_kbps": snap.get("output_kbps", 0),
        "up": snap.get("up", False),
    }
