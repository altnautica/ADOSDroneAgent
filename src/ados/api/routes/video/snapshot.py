"""Snapshot routes: GET /video/snapshot.jpg + POST /video/snapshot."""

from __future__ import annotations

import asyncio
from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, Response

from ados.api.deps import get_agent_app

from ._common import _get_video_pipeline

router = APIRouter()


def _fresh_position(state: object | None) -> tuple[float | None, float | None]:
    """The vehicle's position, or `(None, None)` when it cannot be trusted.

    A snapshot's geotag is written into the JPEG's EXIF, which makes it a
    DURABLE artifact: a stale telemetry reading on a live surface is
    corrected by the next sample, but a wrong coordinate baked into a photo
    is wrong forever and will be believed later by someone with no way to
    know the fix was frozen.

    Both callers used to read `state.lat`/`state.lon` with no gate at all,
    falling back to `0.0` when there was no state — so a node with a dead
    GPS geotagged every photo at the last place it saw, and a node with no
    telemetry at all geotagged them at 0°N 0°E, which is a real place in the
    Gulf of Guinea rather than an obvious sentinel.

    `position_fresh` is false when the fix age is unknown, so "cannot tell"
    never reads as "current".
    """
    if state is None:
        return (None, None)
    if not getattr(state, "position_fresh", False):
        return (None, None)
    lat = getattr(state, "lat", None)
    lon = getattr(state, "lon", None)
    if not isinstance(lat, (int, float)) or isinstance(lat, bool):
        return (None, None)
    if not isinstance(lon, (int, float)) or isinstance(lon, bool):
        return (None, None)
    return (float(lat), float(lon))


@router.get("/video/snapshot.jpg")
async def get_snapshot_jpg(request: Request) -> Response:
    """Serve the most-recent JPEG snapshot as image/jpeg.

    Used by the dashboard video panel as the final fallback when both
    WebRTC WHEP and HLS playback fail. The endpoint:
    1. Looks for the latest snapshot in the recording dir.
    2. If none exists, captures a fresh one (synchronously).
    3. Returns the bytes with image/jpeg content-type.
    """
    pipeline = _get_video_pipeline()
    app = get_agent_app()

    snapshot_dir: Path | None = None
    try:
        recording_dir = app.config.video.recording.path
        snapshot_dir = Path(recording_dir.rstrip("/")) / "snapshots"
    except Exception:  # noqa: BLE001
        pass

    # Try the latest existing snapshot first — fast path, no camera I/O.
    if snapshot_dir and snapshot_dir.is_dir():
        latest = max(
            (p for p in snapshot_dir.glob("*.jpg")),
            key=lambda p: p.stat().st_mtime,
            default=None,
        )
        if latest is not None and latest.is_file():
            return FileResponse(
                str(latest),
                media_type="image/jpeg",
                headers={"Cache-Control": "no-store"},
            )

    # No cached snapshot — capture a fresh one inline.
    if pipeline is None:
        raise HTTPException(status_code=404, detail="no snapshot available")

    if hasattr(pipeline, "capture_snapshot"):
        result = pipeline.capture_snapshot()
        path = await result if asyncio.iscoroutine(result) else result
        if path and Path(path).is_file():
            return FileResponse(
                path, media_type="image/jpeg",
                headers={"Cache-Control": "no-store"},
            )

    cam_mgr = getattr(pipeline, "camera_manager", None)
    primary = cam_mgr.get_primary() if cam_mgr else None
    if primary is None:
        raise HTTPException(status_code=404, detail="no primary camera")

    from ados.services.video.snapshot import capture_snapshot

    gps_lat, gps_lon = _fresh_position(app.vehicle_state())
    if snapshot_dir is None:
        raise HTTPException(status_code=500, detail="snapshot dir unavailable")
    path = await capture_snapshot(primary, str(snapshot_dir), gps_lat, gps_lon)
    if not path or not Path(path).is_file():
        raise HTTPException(status_code=500, detail="snapshot capture failed")
    return FileResponse(
        path, media_type="image/jpeg", headers={"Cache-Control": "no-store"},
    )


@router.post("/video/snapshot")
async def trigger_snapshot():
    """Capture a JPEG snapshot from the primary camera."""
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {"error": "video pipeline not initialized", "path": ""}

    # For demo pipelines, use the demo capture method
    if hasattr(pipeline, "capture_snapshot"):
        result = pipeline.capture_snapshot()
        # Handle both sync and async capture methods
        if asyncio.iscoroutine(result):
            path = await result
        else:
            path = result
        if path:
            return {"path": path, "status": "captured"}
        return {"error": "capture failed", "path": ""}

    # For real pipelines, use the snapshot module
    cam_mgr = pipeline.camera_manager
    primary = cam_mgr.get_primary()
    if primary is None:
        return {"error": "no primary camera", "path": ""}

    from ados.services.video.snapshot import capture_snapshot

    app = get_agent_app()
    gps_lat, gps_lon = _fresh_position(app.vehicle_state())

    recording_dir = app.config.video.recording.path
    snapshot_dir = recording_dir.rstrip("/") + "/snapshots"

    path = await capture_snapshot(primary, snapshot_dir, gps_lat, gps_lon)
    if path and Path(path).is_file():
        return {"path": path, "status": "captured"}
    return {"error": "capture failed or file not found", "path": str(path or "")}


__all__ = ["router", "get_snapshot_jpg", "trigger_snapshot"]
