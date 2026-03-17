"""OTA update API routes."""

from __future__ import annotations

from fastapi import APIRouter, Query
from pydantic import BaseModel

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter()


class OtaCheckResponse(BaseModel):
    """Response model for OTA check."""

    status: str
    version: str | None = None
    changelog: str | None = None
    file_size: int | None = None


class OtaActionResponse(BaseModel):
    """Response model for OTA install/rollback actions."""

    status: str
    message: str


@router.get("/ota")
async def get_ota_status():
    """Current OTA state: version, update status, download progress."""
    app = get_agent_app()
    if hasattr(app, "ota_updater") and app.ota_updater is not None:
        return app.ota_updater.get_status()
    return {
        "state": "idle",
        "current_version": __version__,
        "channel": "stable",
        "github_repo": "altnautica/ADOSDroneAgent",
        "last_check": "",
        "previous_version": "",
        "error": "",
        "pending_update": None,
        "download": {
            "state": "idle",
            "percent": 0.0,
            "bytes_downloaded": 0,
            "total_bytes": 0,
            "speed_bps": 0,
            "eta_seconds": 0,
        },
    }


@router.post("/ota/check")
async def trigger_check():
    """Trigger a manual update check."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return OtaActionResponse(status="error", message="OTA service not available")

    manifest = await app.ota_updater.check()
    if manifest:
        return OtaCheckResponse(
            status="update_available",
            version=manifest.version,
            changelog=manifest.changelog,
            file_size=manifest.file_size,
        )
    return OtaCheckResponse(status="up_to_date")


@router.post("/ota/install")
async def trigger_install():
    """Start download and installation of a pending update."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return OtaActionResponse(status="error", message="OTA service not available")

    ok = await app.ota_updater.download_and_verify()
    if not ok:
        return OtaActionResponse(status="error", message=app.ota_updater.error)

    ok = await app.ota_updater.install()
    if not ok:
        return OtaActionResponse(status="error", message=app.ota_updater.error)

    return OtaActionResponse(status="installed", message="Update installed successfully.")


@router.post("/ota/restart")
async def trigger_restart():
    """Restart the agent service after an update."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return OtaActionResponse(status="error", message="OTA service not available")

    ok = await app.ota_updater.restart_service()
    if ok:
        return OtaActionResponse(status="restarting", message="Service restart initiated.")
    return OtaActionResponse(status="info", message="Not on Linux. Restart the agent manually.")


@router.post("/ota/rollback")
async def trigger_rollback(version: str | None = Query(default=None)):
    """Rollback to a previous version via pip install from PyPI."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return OtaActionResponse(status="error", message="OTA service not available")

    success = await app.ota_updater.rollback(version)
    if success:
        target = version or "previous"
        return OtaActionResponse(
            status="rolled_back", message=f"Rolled back to {target}. Service restarting."
        )
    return OtaActionResponse(
        status="error", message=app.ota_updater.error or "Rollback failed."
    )
