"""GET /video — composite stream status route.

Returns the camera list, recorder state, mediamtx state, and the
derived WHEP URL. Two execution paths exist depending on whether the
VideoPipeline lives in this process (single-process / bench dev) or
in the dedicated ``ados-video`` service (production multi-process).
"""

from __future__ import annotations

from fastapi import APIRouter, Request

from ados.api.deps import get_agent_app

from ._common import (
    _MEDIAMTX_WEBRTC_PORT,
    _empty_recording_block,
    _get_video_pipeline,
    _probe_mediamtx,
    _probe_mediamtx_via_whep,
    _recording_block,
)

router = APIRouter()


def _discover_cameras_for_api() -> dict:
    """Run a fresh HAL camera discovery for the API response.

    The live camera_mgr assignment lives in the ados-video process and
    is not directly readable from the API process. Re-running the HAL
    discovery is cheap (~150ms) and gives the operator the same view
    the wizard's hardware-check step shows so the Video step's
    "Detected cameras" panel is not silently empty when a camera IS
    plugged in. Returns the same shape camera_mgr.to_dict() returns
    with assignments left empty (we cannot infer those without IPC).
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


@router.get("/video")
async def get_video_status(request: Request):
    """Video pipeline status: cameras, streams, recording, mediamtx, WHEP URL."""
    from ados.core.deps import check_video_dependencies

    deps = check_video_dependencies()
    deps_dict = {d.name: {"found": d.found, "path": d.path} for d in deps}

    pipeline = _get_video_pipeline()

    # Multi-process mode: pipeline is None because ados-video owns it.
    # Probe mediamtx directly to determine video state. The camera list
    # comes from a fresh HAL discovery so the operator sees what the
    # agent thinks is plugged in even though the live camera_mgr
    # assignments live in the ados-video process and are not directly
    # readable from here without IPC.
    if pipeline is None:
        cameras_payload = _discover_cameras_for_api()
        mtx = await _probe_mediamtx()
        if mtx is None or not mtx.get("ready"):
            # Ground-station-profile MediaMTX puts auth on the management
            # API; the WHEP probe doesn't depend on it. Fall through so
            # the REST surface reports running when the WHEP endpoint is
            # actually serving frames.
            mtx = await _probe_mediamtx_via_whep() or mtx
        recording_block = _empty_recording_block()
        if mtx and mtx.get("ready"):
            host = request.headers.get("host", "localhost").split(":")[0]
            whep_url = f"http://{host}:{_MEDIAMTX_WEBRTC_PORT}/main/whep"
            return {
                "state": "running",
                "cameras": cameras_payload,
                "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
                "mediamtx": mtx,
                "whep_url": whep_url,
                "dependencies": deps_dict,
                **recording_block,
            }
        return {
            "state": "not_initialized",
            "cameras": cameras_payload,
            "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
            "mediamtx": {"running": False},
            "whep_url": None,
            "dependencies": deps_dict,
            **recording_block,
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
    # Surface the recording state at the top level so the LCD video page
    # and the GCS can read it without re-implementing the recorder
    # serializer.
    status.update(_recording_block(pipeline))
    return status


__all__ = ["router", "_discover_cameras_for_api", "get_video_status"]
# Re-export for the convenience of any caller that historically pulled
# the get_agent_app symbol from the route module's namespace.
get_agent_app = get_agent_app
