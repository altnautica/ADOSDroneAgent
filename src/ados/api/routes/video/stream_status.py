"""GET /video — composite stream status route.

Returns the camera list, mediamtx state, the binary dependencies and the
derived WHEP/HLS URLs. The encoder and its recorder run in the native video
service, so this route reports only what it can observe: what is plugged in,
whether mediamtx has a live publisher, and which tools are installed. It does
not report a recording state it cannot see.
"""

from __future__ import annotations

import asyncio

from fastapi import APIRouter

from ._common import _probe_mediamtx, _probe_mediamtx_via_whep

router = APIRouter()


def _discover_cameras_for_api() -> dict:
    """Run a fresh HAL camera discovery for the API response.

    The live role assignment lives in the video service and is not readable
    from here, so ``assignments`` is left empty. The discovery shells out to
    the V4L2 tools; the caller runs it off the event loop.
    """
    try:
        from ados.hal.camera import discover_cameras

        cams = discover_cameras()
        return {
            "cameras": [c.to_dict() for c in cams],
            "assignments": {},
        }
    except Exception:
        return {"cameras": [], "assignments": {}}


def _dependencies() -> dict:
    from ados.core.deps import check_video_dependencies

    return {
        d.name: {"found": d.found, "path": d.path}
        for d in check_video_dependencies()
    }


@router.get("/video")
async def get_video_status():
    """Video pipeline status: cameras, mediamtx, dependencies, WHEP/HLS URLs."""
    cameras_payload, deps_dict = await asyncio.gather(
        asyncio.to_thread(_discover_cameras_for_api),
        asyncio.to_thread(_dependencies),
    )
    mtx = await _probe_mediamtx()
    if mtx is None or not mtx.get("ready"):
        # Ground-station-profile MediaMTX puts auth on the management
        # API; the WHEP probe doesn't depend on it.
        mtx = await _probe_mediamtx_via_whep() or mtx
    if mtx and mtx.get("ready"):
        return {
            "state": "running",
            "cameras": cameras_payload,
            "mediamtx": mtx,
            "whep_url": "/whep",
            "hls_url": "/hls/main/index.m3u8",
            "dependencies": deps_dict,
        }
    return {
        "state": "not_initialized",
        "cameras": cameras_payload,
        "mediamtx": {"running": bool(mtx and mtx.get("running"))},
        "whep_url": None,
        "hls_url": None,
        "dependencies": deps_dict,
    }


__all__ = ["router", "_discover_cameras_for_api", "get_video_status"]
