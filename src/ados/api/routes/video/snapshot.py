"""Snapshot routes: GET /video/snapshot.jpg + POST /video/snapshot."""

from __future__ import annotations

import asyncio
from pathlib import Path

from fastapi import APIRouter, HTTPException
from fastapi.responses import FileResponse

from ados.api.deps import get_agent_app

router = APIRouter()


def _latest_snapshot(snapshot_dir: Path) -> Path | None:
    """The newest JPEG in the snapshot directory, or None when there is none."""
    if not snapshot_dir.is_dir():
        return None
    return max(
        (p for p in snapshot_dir.glob("*.jpg") if p.is_file()),
        key=lambda p: p.stat().st_mtime,
        default=None,
    )


@router.get("/video/snapshot.jpg")
async def get_snapshot_jpg() -> FileResponse:
    """Serve the most-recent JPEG snapshot as image/jpeg.

    Used by the dashboard video panel as the final fallback when both WebRTC
    WHEP and HLS playback fail. It serves the newest file under the recording
    directory's ``snapshots/``; the camera itself is owned by the native video
    service, so there is no capture from here, only a 404 when none exists.
    """
    app = get_agent_app()
    recording_dir = str(app.config.video.recording.path)
    snapshot_dir = Path(recording_dir.rstrip("/")) / "snapshots"
    latest = await asyncio.to_thread(_latest_snapshot, snapshot_dir)
    if latest is None:
        raise HTTPException(status_code=404, detail="no snapshot available")
    return FileResponse(
        str(latest),
        media_type="image/jpeg",
        headers={"Cache-Control": "no-store"},
    )


@router.post("/video/snapshot")
async def trigger_snapshot():
    """Refuse: the camera is owned by the native video service."""
    raise HTTPException(
        status_code=501,
        detail={
            "error": {
                "code": "E_NOT_ON_THIS_SURFACE",
                "message": "Snapshot capture is not served by this surface on this node.",
            }
        },
    )


__all__ = ["router", "get_snapshot_jpg", "trigger_snapshot"]
