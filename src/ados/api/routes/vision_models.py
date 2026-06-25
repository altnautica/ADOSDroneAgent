# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Vision model registry + download API routes.

Provides:
  - GET  /api/vision/models                          list registry + installed + custom + active + cache usage
  - POST /api/vision/models/{model_id}/download     pick the best variant for this board
  - GET  /api/vision/models/{model_id}/status       download progress + installed state

The model cache is decoupled from any specific autonomy feature; plugins
that need an inference model load via the same registry.

The active-detector selection and the custom-model upload are control-plane
writes served by the native front; this read route projects them back so the
GCS sees which model is active and lists operator-uploaded models alongside the
registry and the on-disk files.
"""

from __future__ import annotations

from typing import Any

import yaml
from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.core.paths import CONFIG_YAML

router = APIRouter()


def _active_detector_model_id() -> str | None:
    """The currently-selected detector model id, read straight from the config.

    The native ``PUT /api/vision/detector`` route writes ``vision.detector`` into
    ``/etc/ados/config.yaml`` (a block the Python ``VisionConfig`` model does not
    declare, so it is dropped on the typed load); the raw YAML is the source of
    truth. A missing file / block / id yields ``None``.
    """
    try:
        with open(str(CONFIG_YAML), encoding="utf-8") as fh:
            data = yaml.safe_load(fh)
    except (OSError, yaml.YAMLError):
        return None
    if not isinstance(data, dict):
        return None
    vision = data.get("vision")
    if not isinstance(vision, dict):
        return None
    detector = vision.get("detector")
    if not isinstance(detector, dict):
        return None
    model_id = detector.get("model_id")
    return model_id if isinstance(model_id, str) and model_id else None


@router.get("/vision/models")
async def list_vision_models() -> dict[str, Any]:
    """List available, installed, and custom vision models plus the active one."""
    active = _active_detector_model_id()
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {
            "registry": [],
            "installed": [],
            "custom": [],
            "active": active,
            "cache": {"used_bytes": 0, "max_bytes": 0, "used_mb": 0, "max_mb": 0},
        }

    # Refresh registry (uses ETag caching, fast on 304)
    await mm.fetch_registry()

    return {
        "registry": [m.to_dict() for m in mm.registry],
        "installed": mm.list_installed(),
        "custom": mm.list_custom(),
        "active": active,
        "cache": mm.get_cache_usage(),
    }


@router.post("/vision/models/{model_id}/download")
async def download_vision_model(model_id: str):
    """Download a vision model, selecting the best variant for this board."""
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {"status": "error", "message": "Model manager not available"}

    await mm.fetch_registry()

    try:
        path = await mm.download_model(model_id)
        return {"status": "ok", "message": f"Model {model_id} downloaded", "path": path}
    except ValueError as exc:
        return {"status": "error", "message": str(exc)}
    except Exception as exc:
        return {"status": "error", "message": f"Download failed: {exc}"}


@router.get("/vision/models/{model_id}/status")
async def get_model_status(model_id: str):
    """Get download progress and installed status for a model."""
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {"installed": False, "download": None}

    installed = False
    for m in mm.list_installed():
        if m["id"] == model_id:
            installed = True
            break

    progress = mm.get_download_progress(model_id)
    return {
        "installed": installed,
        "download": progress.to_dict(),
    }
