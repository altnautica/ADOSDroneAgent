"""Survey REST API routes.

All endpoints are under /api/v1/survey/*.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/v1/survey", tags=["survey"])


def _get_survey_service():
    """Get the running SurveyService instance via agent app."""
    try:
        from ados.api.deps import get_agent_app
        app = get_agent_app()
        svc = getattr(app, "_survey_service", None)
        return svc
    except Exception:
        return None


@router.get("/status")
async def survey_status():
    """Return current survey mission status and quality counters."""
    svc = _get_survey_service()
    if svc is None:
        return {
            "active": False,
            "mission_id": None,
            "captured_frames": 0,
            "pass_frames": 0,
            "warn_frames": 0,
            "fail_frames": 0,
            "coverage_pct": 0,
            "service": "unavailable",
        }
    return {**svc.current_status(), "service": "running"}


@router.get("/datasets")
async def list_datasets():
    """List available survey datasets."""
    return {"datasets": [], "note": "Dataset packaging available in Phase 3b complete build"}


@router.get("/templates")
async def list_templates():
    """List built-in mission templates."""
    return {
        "templates": [
            {"name": "grid_survey", "description": "Standard grid photogrammetry"},
            {"name": "corridor_survey", "description": "Linear corridor mapping"},
            {"name": "perimeter_survey", "description": "Area perimeter scan"},
            {"name": "facade_scan", "description": "Building facade 3D scan"},
            {"name": "point_survey", "description": "Single point inspection"},
        ]
    }


@router.get("/health")
async def health():
    """Survey service health."""
    svc = _get_survey_service()
    return {"status": "running" if svc else "unavailable"}
