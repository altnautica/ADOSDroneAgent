"""Service status routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/services")
async def list_services():
    """List all running services with status."""
    app = get_agent_app()
    services = []

    for task in app._tasks:
        services.append({
            "name": task.get_name(),
            "status": "running" if not task.done() else "stopped",
            "cancelled": task.cancelled(),
        })

    return {"services": services}
