# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Features and vision model management API routes."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel

from ados.api.deps import get_agent_app

router = APIRouter()


class FeatureActionResponse(BaseModel):
    """Response model for feature enable/disable/activate/deactivate."""

    status: str
    message: str


class ParamsBody(BaseModel):
    """Request body for updating feature parameters."""

    params: dict[str, Any]


@router.get("/capabilities")
async def get_capabilities():
    """Full device capabilities: tier, cameras, compute, vision, models, features."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return {"status": "error", "message": "Feature manager not available"}
    return fm.get_capabilities()


@router.post("/features/{feature_id}/enable")
async def enable_feature(feature_id: str):
    """Enable a feature (persisted across reboots)."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return FeatureActionResponse(status="error", message="Feature manager not available")
    result = fm.enable(feature_id)
    return result


@router.post("/features/{feature_id}/disable")
async def disable_feature(feature_id: str):
    """Disable a feature (persisted across reboots)."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return FeatureActionResponse(status="error", message="Feature manager not available")
    result = fm.disable(feature_id)
    return result


@router.post("/features/{feature_id}/activate")
async def activate_feature(feature_id: str):
    """Activate a feature at runtime (start processing)."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return FeatureActionResponse(status="error", message="Feature manager not available")
    result = fm.activate(feature_id)
    return result


@router.post("/features/{feature_id}/deactivate")
async def deactivate_feature(feature_id: str):
    """Deactivate a feature at runtime (stop processing)."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return FeatureActionResponse(status="error", message="Feature manager not available")
    result = fm.deactivate(feature_id)
    return result


@router.put("/features/{feature_id}/params")
async def update_feature_params(feature_id: str, body: ParamsBody):
    """Update runtime parameters for a feature."""
    app = get_agent_app()
    fm = getattr(app, "feature_manager", None)
    if fm is None:
        return {"status": "error", "message": "Feature manager not available"}
    result = fm.set_params(feature_id, body.params)
    return result


@router.get("/vision/models")
async def list_vision_models():
    """List available and installed vision models."""
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {
            "registry": [],
            "installed": [],
            "cache": {"used_bytes": 0, "max_bytes": 0, "used_mb": 0, "max_mb": 0},
        }

    # Refresh registry (uses ETag caching, fast on 304)
    await mm.fetch_registry()

    return {
        "registry": [m.to_dict() for m in mm.registry],
        "installed": mm.list_installed(),
        "cache": mm.get_cache_usage(),
    }


@router.post("/vision/models/{model_id}/download")
async def download_vision_model(model_id: str):
    """Download a vision model, selecting the best variant for this board."""
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {"status": "error", "message": "Model manager not available"}

    # Make sure registry is loaded
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

    # Check if installed
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
