"""Suite management stub routes."""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/suites")
async def list_suites():
    """List available suites."""
    return []


@router.post("/suites/{suite_id}/install")
async def install_suite(suite_id: str):
    """Install a suite (not yet implemented)."""
    return {"status": "not_implemented", "message": f"Suite install not yet available: {suite_id}"}


@router.post("/suites/{suite_id}/uninstall")
async def uninstall_suite(suite_id: str):
    """Uninstall a suite (not yet implemented)."""
    return {"status": "not_implemented", "message": f"Suite uninstall not yet available: {suite_id}"}


@router.post("/suites/{suite_id}/activate")
async def activate_suite(suite_id: str):
    """Activate a suite (not yet implemented)."""
    return {"status": "not_implemented", "message": f"Suite activate not yet available: {suite_id}"}
