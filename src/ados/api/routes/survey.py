"""Survey REST API routes.

All endpoints are under /api/v1/survey/*.
Routes query the ados-survey.service via its local state file
rather than through AgentApp, because the survey service runs
as a separate supervised process.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from fastapi import APIRouter, HTTPException

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/v1/survey", tags=["survey"])

# The ados-survey.service writes its current status here on each update.
# Routes read from this file to report status to the GCS. If the service
# is not running, the file is stale or missing; routes return
# {"service": "unavailable"}.
STATUS_FILE = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "survey_status.json"


def _read_status() -> dict | None:
    """Read the current survey service status from the shared file."""
    if not STATUS_FILE.exists():
        return None
    try:
        data = json.loads(STATUS_FILE.read_text())
        # Guard against stale data — treat anything older than 30s as offline.
        import time
        if time.time() - data.get("ts", 0) > 30:
            return None
        return data
    except Exception:
        return None


@router.get("/status")
async def survey_status():
    """Return current survey mission status and quality counters."""
    status = _read_status()
    if status is None:
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
    return {**status, "service": "running"}


@router.get("/datasets")
async def list_datasets():
    """List available survey datasets."""
    return {
        "datasets": [],
        "note": "Dataset packaging (ODM/COLMAP/nerfstudio export) is part of v1.1.",
    }


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
    return {"status": "running" if _read_status() else "unavailable"}
