"""Video pipeline API routes."""

from __future__ import annotations

import asyncio
from pathlib import Path

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger

log = get_logger("api.video")

router = APIRouter()


def _get_video_pipeline():
    """Retrieve the video pipeline from the agent app.

    Returns the pipeline object or None if not initialized.
    """
    app = get_agent_app()
    return getattr(app, "_video_pipeline", None)


@router.get("/video")
async def get_video_status():
    """Video pipeline status: cameras, streams, recording, mediamtx."""
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {
            "state": "not_initialized",
            "cameras": {"cameras": [], "assignments": {}},
            "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
            "mediamtx": {"running": False},
        }
    return pipeline.get_status()


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
    state = getattr(app, "_vehicle_state", None)
    gps_lat = state.lat if state else 0.0
    gps_lon = state.lon if state else 0.0

    recording_dir = app.config.video.recording.path
    snapshot_dir = recording_dir.rstrip("/") + "/snapshots"

    path = await capture_snapshot(primary, snapshot_dir, gps_lat, gps_lon)
    if path and Path(path).is_file():
        return {"path": path, "status": "captured"}
    return {"error": "capture failed or file not found", "path": str(path or "")}


@router.post("/video/record/start")
async def start_recording():
    """Start MP4 recording from the primary camera."""
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {"error": "video pipeline not initialized", "path": ""}

    if hasattr(pipeline, "start_recording") and callable(pipeline.start_recording):
        # Demo pipeline has sync start_recording
        path = pipeline.start_recording()
        return {"path": path, "status": "recording"}

    # Real pipeline: use recorder
    recorder = pipeline.recorder
    if recorder.recording:
        return {"path": recorder.current_path, "status": "already_recording"}

    path = await recorder.start_recording()
    if path:
        return {"path": path, "status": "recording"}
    return {"error": "failed to start recording", "path": ""}


@router.post("/video/record/stop")
async def stop_recording():
    """Stop the current MP4 recording."""
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {"error": "video pipeline not initialized", "path": ""}

    if hasattr(pipeline, "stop_recording") and callable(pipeline.stop_recording):
        path = pipeline.stop_recording()
        return {"path": path, "status": "stopped"}

    recorder = pipeline.recorder
    if not recorder.recording:
        return {"error": "no active recording", "path": ""}

    path = await recorder.stop_recording()
    return {"path": path, "status": "stopped"}
