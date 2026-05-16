"""Camera enumeration + role-switch routes."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException

from ._common import CameraSwitchBody, _get_video_pipeline

router = APIRouter()


def _enumerate_cameras(pipeline: Any) -> dict[str, Any]:
    """Return cameras + assignments in the API contract shape.

    When the pipeline is live we read the camera_manager directly so the
    response includes the operator's current role bindings. Otherwise we
    fall back to a fresh HAL discovery (the same fallback used for the
    /api/video status response) and return empty assignments.
    """
    if pipeline is not None:
        cam_mgr = getattr(pipeline, "camera_manager", None)
        if cam_mgr is not None:
            cameras = [
                {
                    "device_path": c.device_path,
                    "type": c.type.value,
                    "label": c.name,
                    "width": c.width,
                    "height": c.height,
                }
                for c in cam_mgr.cameras
            ]
            assignments: dict[str, str] = {}
            for role, cam in cam_mgr.assignments.items():
                assignments[role.value] = cam.device_path
            return {"cameras": cameras, "assignments": assignments}

    # Pipeline not live: fall back to a one-shot HAL discovery so the
    # operator still sees what's plugged in.
    try:
        from ados.hal.camera import discover_cameras

        cams = discover_cameras()
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
    except Exception:
        return {"cameras": [], "assignments": {}}


@router.get("/video/cameras")
async def list_cameras():
    """Enumerate cameras + their current role assignments.

    Always returns 200 with at least an empty ``cameras`` array. When
    the video pipeline is live the response reflects camera_manager
    state so the operator's role bindings show through; otherwise we
    fall back to a fresh HAL discovery so a not-yet-running pipeline
    doesn't make the UI look like the SBC has no cameras attached.
    """
    pipeline = _get_video_pipeline()
    return _enumerate_cameras(pipeline)


@router.post("/video/camera/switch")
async def switch_camera(body: CameraSwitchBody):
    """Reassign a camera role and restart the encoder.

    Validates that ``device_path`` matches an enumerated camera before
    the role assignment is persisted. Returns 400 when the device path
    is unknown so the LCD page and the GCS can surface a precise
    rejection reason instead of a vague 500.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        raise HTTPException(
            status_code=503, detail="video pipeline not initialized"
        )

    cam_mgr = getattr(pipeline, "camera_manager", None)
    if cam_mgr is None:
        raise HTTPException(
            status_code=503, detail="camera manager unavailable"
        )

    known_paths = {c.device_path for c in cam_mgr.cameras}
    if body.device_path not in known_paths:
        raise HTTPException(status_code=400, detail="unknown camera")

    # The pipeline owns the lock + serialization; surface its errors as
    # 400 (lookup) or 500 (everything else).
    try:
        await pipeline.restart_with_camera(body.role, body.device_path)
    except LookupError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc

    return {"ok": True, "restarting": True}


__all__ = ["router", "_enumerate_cameras", "list_cameras", "switch_camera"]
