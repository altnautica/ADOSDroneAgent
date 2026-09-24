"""Camera enumeration + role-switch routes."""

from __future__ import annotations

import asyncio
from typing import Any

from fastapi import APIRouter, HTTPException

router = APIRouter()


def _enumerate_cameras() -> dict[str, Any]:
    """Return a fresh HAL discovery in the API contract shape.

    The role bindings live in the native video service's roster, which this
    process does not hold, so ``assignments`` is empty: the list says what is
    plugged in, not which role each device serves.
    """
    try:
        from ados.hal.camera import discover_cameras

        cams = discover_cameras()
    except Exception:  # noqa: BLE001 — an unreadable bus lists nothing
        return {"cameras": [], "assignments": {}}
    return {
        "cameras": [
            {
                "device_path": c.device_path,
                "type": c.type.value,
                "label": c.name,
                "width": c.width,
                "height": c.height,
            }
            for c in cams
        ],
        "assignments": {},
    }


@router.get("/video/cameras")
async def list_cameras():
    """Enumerate the cameras plugged into this node.

    The discovery shells out to the V4L2 tools, so it runs off the event loop.
    """
    return await asyncio.to_thread(_enumerate_cameras)


@router.post("/video/camera/switch")
async def switch_camera():
    """Camera roles are assigned through the native video roster.

    The encoder and its camera bindings run in the native video service; this
    surface cannot rebind them, so it says so rather than pretending to.
    """
    raise HTTPException(
        status_code=501,
        detail={
            "error": {
                "code": "E_NOT_ON_THIS_SURFACE",
                "message": (
                    "Camera roles are set through the video roster "
                    "(PUT /api/video/roster)."
                ),
            }
        },
    )


__all__ = ["router", "list_cameras", "switch_camera"]
