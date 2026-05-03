"""Video pipeline API routes."""

from __future__ import annotations

import asyncio
from pathlib import Path

import httpx
from fastapi import APIRouter, Request

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger

log = get_logger("api.video")

router = APIRouter()

# mediamtx default ports — must match the values in mediamtx.py.
_MEDIAMTX_API_PORT = 9997
_MEDIAMTX_WEBRTC_PORT = 8889


def _get_video_pipeline():
    """Retrieve the video pipeline from the agent app.

    Returns the pipeline object or None if not initialized.
    """
    app = get_agent_app()
    return app.video_pipeline()


async def _probe_mediamtx() -> dict | None:
    """Check if mediamtx is alive by hitting its local API.

    In multi-process mode the VideoPipeline object lives in the ados-video
    service, not in ados-api.  The API service therefore cannot call
    pipeline.get_status().  Instead we probe mediamtx's REST API at
    localhost:9997 to determine whether a stream is active.

    Returns a small dict with stream metadata or None if mediamtx is
    unreachable / has no active streams.
    """
    try:
        async with httpx.AsyncClient(timeout=2.0) as client:
            resp = await client.get(f"http://127.0.0.1:{_MEDIAMTX_API_PORT}/v3/paths/list")
            if resp.status_code != 200:
                return None
            data = resp.json()
            items = data.get("items", [])
            if not items:
                return None
            path = items[0]
            return {
                "running": True,
                "stream_name": path.get("name", "main"),
                "ready": path.get("ready", False),
                "tracks": path.get("tracks", []),
                "readers": len(path.get("readers", [])),
                "webrtc_port": _MEDIAMTX_WEBRTC_PORT,
            }
    except Exception:
        return None


@router.get("/video")
async def get_video_status(request: Request):
    """Video pipeline status: cameras, streams, recording, mediamtx, WHEP URL."""
    from ados.core.deps import check_video_dependencies

    deps = check_video_dependencies()
    deps_dict = {d.name: {"found": d.found, "path": d.path} for d in deps}

    pipeline = _get_video_pipeline()

    # Multi-process mode: pipeline is None because ados-video owns it.
    # Probe mediamtx directly to determine video state.
    if pipeline is None:
        mtx = await _probe_mediamtx()
        if mtx and mtx.get("ready"):
            host = request.headers.get("host", "localhost").split(":")[0]
            whep_url = f"http://{host}:{_MEDIAMTX_WEBRTC_PORT}/main/whep"
            return {
                "state": "running",
                "cameras": {"cameras": [], "assignments": {}},
                "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
                "mediamtx": mtx,
                "whep_url": whep_url,
                "dependencies": deps_dict,
            }
        return {
            "state": "not_initialized",
            "cameras": {"cameras": [], "assignments": {}},
            "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
            "mediamtx": {"running": False},
            "whep_url": None,
            "dependencies": deps_dict,
        }

    status = pipeline.get_status()

    # Construct WHEP URL from mediamtx state
    if status.get("mediamtx", {}).get("running"):
        webrtc_port = status["mediamtx"].get("webrtc_port", 8889)
        host = request.headers.get("host", "localhost").split(":")[0]
        status["whep_url"] = f"http://{host}:{webrtc_port}/main/whep"
    else:
        status["whep_url"] = None

    status["dependencies"] = deps_dict
    return status


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
    state = app.vehicle_state()
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
