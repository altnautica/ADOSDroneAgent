"""OTA update API routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/ota")
async def get_ota_status():
    """Current OTA state: version, update status, slot info, download progress."""
    app = get_agent_app()
    if hasattr(app, "ota_updater") and app.ota_updater is not None:
        return app.ota_updater.get_status()
    return {
        "state": "idle",
        "current_version": "0.1.0",
        "error": "",
        "download": {"state": "idle", "percent": 0.0},
        "slots": {},
    }


@router.post("/ota/check")
async def trigger_check():
    """Trigger a manual update check."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return {"status": "error", "message": "OTA service not available"}

    manifest = await app.ota_updater.check()
    if manifest:
        return {
            "status": "update_available",
            "version": manifest.version,
            "changelog": manifest.changelog,
            "file_size": manifest.file_size,
        }
    return {"status": "up_to_date"}


@router.post("/ota/install")
async def trigger_install():
    """Start download and installation of a pending update."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return {"status": "error", "message": "OTA service not available"}

    ok = await app.ota_updater.download_and_verify()
    if not ok:
        return {"status": "error", "message": app.ota_updater.error}

    ok = await app.ota_updater.install()
    if not ok:
        return {"status": "error", "message": app.ota_updater.error}

    return {"status": "installed", "message": "Update installed. Activate to apply."}


@router.post("/ota/rollback")
async def trigger_rollback():
    """Force rollback to the previous partition slot."""
    app = get_agent_app()
    if not hasattr(app, "ota_updater") or app.ota_updater is None:
        return {"status": "error", "message": "OTA service not available"}

    if hasattr(app.ota_updater, "_rollback"):
        success = app.ota_updater._rollback.rollback()
        if success:
            return {"status": "rolled_back", "message": "Rollback complete. Reboot to apply."}
        return {"status": "error", "message": "Rollback failed. Standby slot not available."}

    return {"status": "error", "message": "Rollback not supported"}
