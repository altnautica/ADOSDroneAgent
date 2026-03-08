"""OTA update API routes."""

from __future__ import annotations

from fastapi import APIRouter
from pydantic import BaseModel

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
    """Current OTA state: version, update status, slot info, download progress."""
    app = get_agent_app()
    if hasattr(app, "ota_updater") and app.ota_updater is not None:
        return app.ota_updater.get_status()
    # Fallback matches OtaUpdater.get_status() and DemoOtaUpdater.get_status() format
    return {
        "state": "idle",
        "current_version": "0.1.0",
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
        "slots": {
            "active_slot": {
                "slot_name": "A",
                "version": "0.1.0",
                "status": "ok",
                "boot_count": 0,
            },
            "standby_slot": {
                "slot_name": "B",
                "version": "unknown",
                "status": "empty",
                "boot_count": 0,
            },
            "should_rollback": False,
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

    return OtaActionResponse(status="installed", message="Update installed. Activate to apply.")


@router.post("/ota/rollback")
async def trigger_rollback():
    """Force rollback to the previous partition slot."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return OtaActionResponse(status="error", message="OTA service not available")

    if hasattr(app.ota_updater, "_rollback"):
        success = app.ota_updater._rollback.rollback()
        if success:
            return OtaActionResponse(
                status="rolled_back", message="Rollback complete. Reboot to apply."
            )
        return OtaActionResponse(
            status="error", message="Rollback failed. Standby slot not available."
        )

    return OtaActionResponse(status="error", message="Rollback not supported")
