"""Recording routes: POST /video/record/{start,stop}.

Both are mutex-guarded via the shared ``_RECORD_LOCK`` so a concurrent
toggle from the LCD page and the GCS cannot interleave and leave the
recorder in an inconsistent state.
"""

from __future__ import annotations

from fastapi import APIRouter

from ._common import (
    _RECORD_LOCK,
    _empty_recording_block,
    _get_video_pipeline,
    _recording_block,
)

router = APIRouter()


@router.post("/video/record/start")
async def start_recording():
    """Start MP4 recording from the primary camera.

    Mutex-guarded so concurrent toggles from the LCD page and the GCS
    cannot race each other into a half-started recorder. The response
    surfaces the live ``recording`` flag in addition to the legacy
    ``path`` / ``status`` keys so callers can update their UI without a
    follow-up ``GET /api/video`` poll.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {
            "error": "video pipeline not initialized",
            "path": "",
            **_empty_recording_block(),
        }

    async with _RECORD_LOCK:
        if hasattr(pipeline, "start_recording") and callable(pipeline.start_recording):
            # Demo pipeline has sync start_recording.
            path = pipeline.start_recording()
            return {
                "path": path,
                "status": "recording",
                **_recording_block(pipeline),
            }

        # Real pipeline: use recorder.
        recorder = pipeline.recorder
        if recorder.recording:
            return {
                "path": recorder.current_path,
                "status": "already_recording",
                **_recording_block(pipeline),
            }

        path = await recorder.start_recording()
        if path:
            return {
                "path": path,
                "status": "recording",
                **_recording_block(pipeline),
            }
        return {
            "error": "failed to start recording",
            "path": "",
            **_empty_recording_block(),
        }


@router.post("/video/record/stop")
async def stop_recording():
    """Stop the current MP4 recording.

    Mutex-guarded; see :func:`start_recording`.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {
            "error": "video pipeline not initialized",
            "path": "",
            **_empty_recording_block(),
        }

    async with _RECORD_LOCK:
        if hasattr(pipeline, "stop_recording") and callable(pipeline.stop_recording):
            path = pipeline.stop_recording()
            return {
                "path": path,
                "status": "stopped",
                **_recording_block(pipeline),
            }

        recorder = pipeline.recorder
        if not recorder.recording:
            return {
                "error": "no active recording",
                "path": "",
                **_empty_recording_block(),
            }

        path = await recorder.stop_recording()
        return {
            "path": path,
            "status": "stopped",
            **_recording_block(pipeline),
        }


__all__ = ["router", "start_recording", "stop_recording"]
