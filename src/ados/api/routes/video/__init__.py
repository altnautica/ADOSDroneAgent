"""Video pipeline API routes.

The implementation now lives in per-concern files alongside this
barrel. The package-level ``router`` aggregates them and is mounted
elsewhere with ``prefix="/api"`` so routes like ``/video/cameras``
land at ``/api/video/cameras`` as before.

* ``stream_status.py`` — :data:`router` carrying ``GET /video``
  composite status + the per-process WHEP discovery helpers.
* ``snapshot.py`` — :data:`router` carrying ``GET
  /video/snapshot.jpg`` and ``POST /video/snapshot``.
* ``camera_probe.py`` — :data:`router` carrying ``GET
  /video/cameras`` and ``POST /video/camera/switch``.
* ``encoder_config.py`` — :data:`router` carrying ``GET
  /video/config``, ``POST /video/config`` and the controller-snapshot
  fallbacks.
* ``_common.py`` — shared constants (mediamtx ports), the tuning
  body model, and the mediamtx probes.

"""

from __future__ import annotations

from fastapi import APIRouter

from . import camera_probe as _camera_mod
from . import encoder_config as _encoder_mod
from . import snapshot as _snapshot_mod
from . import stream_status as _status_mod
from ._common import (
    _MEDIAMTX_API_PORT,
    _MEDIAMTX_WEBRTC_PORT,
    VideoConfigBody,
    _probe_mediamtx,
    _probe_mediamtx_via_whep,
)
from .camera_probe import list_cameras, switch_camera
from .encoder_config import (
    _bitrate_controller_snapshot,
    _hop_supervisor_snapshot,
    _read_state_file,
    get_video_config,
    set_video_config,
)
from .snapshot import get_snapshot_jpg, trigger_snapshot
from .stream_status import _discover_cameras_for_api, get_video_status

router = APIRouter()
router.include_router(_status_mod.router)
router.include_router(_snapshot_mod.router)
router.include_router(_camera_mod.router)
router.include_router(_encoder_mod.router)


__all__ = [
    "router",
    "_MEDIAMTX_API_PORT",
    "_MEDIAMTX_WEBRTC_PORT",
    "VideoConfigBody",
    "_probe_mediamtx",
    "_probe_mediamtx_via_whep",
    "_discover_cameras_for_api",
    "_read_state_file",
    "_bitrate_controller_snapshot",
    "_hop_supervisor_snapshot",
    "get_video_status",
    "get_snapshot_jpg",
    "trigger_snapshot",
    "list_cameras",
    "switch_camera",
    "get_video_config",
    "set_video_config",
]
