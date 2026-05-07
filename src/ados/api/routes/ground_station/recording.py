"""Server-side video recording endpoints for the ground-station profile.

Captures the live drone video stream as it arrives at the ground node
and serves the file list back to clients via REST. Recording state is
held in `ados.services.ground_station.recorder.GroundStationRecorder`.

Routes:

* POST /recording/start   {filename_hint?: str}
* POST /recording/stop
* GET  /recording/list
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.routes import ground_station as _gs
from ados.core.logging import get_logger

log = get_logger("api.ground_station.recording")

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# ---------------------------------------------------------------------------
# Pydantic models
# ---------------------------------------------------------------------------


class RecordingStartRequest(BaseModel):
    """POST body for starting a recording."""

    filename_hint: str | None = Field(default=None, max_length=64)


class RecordingFileView(BaseModel):
    """One row in the recordings listing."""

    filename: str
    size_bytes: int
    mtime: float


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _recorder() -> Any:
    """Return the process-wide GroundStationRecorder singleton.

    Lazy import + indirection so tests can monkeypatch
    `ados.api.routes.ground_station._recorder` at the package level.
    """
    from ados.services.ground_station.recorder import get_recorder

    return get_recorder()


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------


@router.post("/recording/start")
async def post_recording_start(req: RecordingStartRequest) -> dict[str, Any]:
    """Start a recording. 409 if already recording, 503 on missing ffmpeg."""
    _gs._require_ground_profile()
    rec = _gs._recorder() if hasattr(_gs, "_recorder") else _recorder()

    from ados.services.ground_station.recorder import RecorderError

    try:
        return await rec.start(filename_hint=req.filename_hint)
    except RecorderError as exc:
        status_code = _error_status_code(exc.code)
        log.warning(
            "recording_start_rejected",
            code=exc.code,
            message=exc.message,
        )
        raise HTTPException(
            status_code=status_code,
            detail={"error": {"code": exc.code, "message": exc.message}},
        ) from exc


@router.post("/recording/stop")
async def post_recording_stop() -> dict[str, Any]:
    """Stop the active recording. 409 if none active."""
    _gs._require_ground_profile()
    rec = _gs._recorder() if hasattr(_gs, "_recorder") else _recorder()

    from ados.services.ground_station.recorder import RecorderError

    try:
        return await rec.stop()
    except RecorderError as exc:
        status_code = _error_status_code(exc.code)
        log.warning(
            "recording_stop_rejected",
            code=exc.code,
            message=exc.message,
        )
        raise HTTPException(
            status_code=status_code,
            detail={"error": {"code": exc.code, "message": exc.message}},
        ) from exc


@router.get("/recording/list")
async def get_recording_list() -> dict[str, Any]:
    """List recordings on disk, newest first.

    Each entry carries `filename`, `size_bytes`, and `mtime` (Unix
    seconds). The `recording` flag mirrors `is_active()` so callers
    can render an "in progress" badge in the same call.
    """
    _gs._require_ground_profile()
    rec = _gs._recorder() if hasattr(_gs, "_recorder") else _recorder()

    items = [r.to_dict() for r in rec.list_recordings()]
    return {
        "recording": rec.is_active(),
        "current_filename": rec.current_filename,
        "items": items,
    }


# ---------------------------------------------------------------------------
# Error mapping
# ---------------------------------------------------------------------------


def _error_status_code(code: str) -> int:
    """Map RecorderError codes to HTTP status codes."""
    if code in ("E_RECORDING_ACTIVE", "E_RECORDING_NOT_ACTIVE"):
        return 409
    if code in (
        "E_FFMPEG_NOT_FOUND",
        "E_RECORDER_SPAWN_FAILED",
        "E_RECORDING_DIR_UNWRITABLE",
    ):
        return 503
    if code == "E_DISK_FULL":
        return 507
    return 500
