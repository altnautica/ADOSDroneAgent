"""Video pipeline API routes.

The implementation now lives in per-concern files alongside this
barrel. The package-level ``router`` aggregates them and is mounted
elsewhere with ``prefix="/api"`` so routes like ``/video/cameras``
land at ``/api/video/cameras`` as before.

* ``stream_status.py`` — :data:`router` carrying ``GET /video``
  composite status + the per-process WHEP discovery helpers.
* ``snapshot.py`` — :data:`router` carrying ``GET
  /video/snapshot.jpg`` and ``POST /video/snapshot``.
* ``recording.py`` — :data:`router` carrying ``POST
  /video/record/start`` and ``POST /video/record/stop``.
* ``camera_probe.py`` — :data:`router` carrying ``GET
  /video/cameras`` and ``POST /video/camera/switch``.
* ``encoder_config.py`` — :data:`router` carrying ``GET
  /video/config``, ``POST /video/config`` and the controller-snapshot
  fallbacks.
* ``latency.py`` — :data:`router` carrying ``GET /video/latency`` and
  ``GET /v1/video/air-pipeline``.
* ``_common.py`` — shared constants (mediamtx ports, record lock),
  Pydantic models, pipeline accessor, recording-block + mediamtx
  probe helpers.

Existing callers
(``from ados.api.routes.video import _empty_recording_block,
_probe_mediamtx, _recording_block, _MEDIAMTX_WEBRTC_PORT,
mediamtx_whep_alive_sync, CameraSwitchBody, switch_camera``)
keep working unchanged.
"""

from __future__ import annotations

from fastapi import APIRouter

from . import camera_probe as _camera_mod
from . import encoder_config as _encoder_mod
from . import latency as _latency_mod
from . import recording as _record_mod
from . import snapshot as _snapshot_mod
from . import stream_status as _status_mod
from ._common import (
    _MEDIAMTX_API_PORT,
    _MEDIAMTX_WEBRTC_PORT,
    _RECORD_LOCK,
    CameraSwitchBody,
    VideoConfigBody,
    _empty_recording_block,
    _get_video_pipeline,
    _probe_mediamtx,
    _probe_mediamtx_via_whep,
    _recording_block,
    mediamtx_whep_alive_sync,
)
from .camera_probe import _enumerate_cameras, list_cameras, switch_camera
from .encoder_config import (
    _bitrate_controller_snapshot,
    _hop_supervisor_snapshot,
    _read_state_file,
    get_video_config,
    set_video_config,
)
from .latency import get_air_pipeline_status, get_video_latency
from .recording import start_recording, stop_recording
from .snapshot import get_snapshot_jpg, trigger_snapshot
from .stream_status import _discover_cameras_for_api, get_video_status

router = APIRouter()
router.include_router(_status_mod.router)
router.include_router(_snapshot_mod.router)
router.include_router(_record_mod.router)
router.include_router(_camera_mod.router)
router.include_router(_encoder_mod.router)
router.include_router(_latency_mod.router)


__all__ = [
    "router",
    # constants
    "_MEDIAMTX_API_PORT",
    "_MEDIAMTX_WEBRTC_PORT",
    "_RECORD_LOCK",
    # models
    "CameraSwitchBody",
    "VideoConfigBody",
    # helpers (common)
    "_get_video_pipeline",
    "_empty_recording_block",
    "_recording_block",
    "_probe_mediamtx",
    "_probe_mediamtx_via_whep",
    "mediamtx_whep_alive_sync",
    # route helpers
    "_discover_cameras_for_api",
    "_enumerate_cameras",
    "_read_state_file",
    "_bitrate_controller_snapshot",
    "_hop_supervisor_snapshot",
    # route handlers
    "get_video_status",
    "get_snapshot_jpg",
    "trigger_snapshot",
    "start_recording",
    "stop_recording",
    "list_cameras",
    "switch_camera",
    "get_video_config",
    "set_video_config",
    "get_video_latency",
    "get_air_pipeline_status",
]
