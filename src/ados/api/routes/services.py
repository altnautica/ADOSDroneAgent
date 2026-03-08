"""Service status routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/services")
async def list_services():
    """List all running services with state machine info."""
    app = get_agent_app()

    # Get state machine data from ServiceTracker
    tracker_data = app.services.to_dict()

    # Merge with asyncio task status for runtime info
    services = []
    task_names = {t.get_name() for t in app._tasks}

    for task in app._tasks:
        name = task.get_name()
        tracked = tracker_data.get(name, {})
        services.append({
            "name": name,
            "state": tracked.get("state", "running" if not task.done() else "stopped"),
            "task_done": task.done(),
            "cancelled": task.cancelled(),
            "last_transition": tracked.get("last_transition", 0),
            "transition_count": tracked.get("transition_count", 0),
        })

    # Include tracked services that might not have an active task
    for name, info in tracker_data.items():
        if name not in task_names:
            services.append({
                "name": name,
                "state": info["state"],
                "task_done": True,
                "cancelled": False,
                "last_transition": info["last_transition"],
                "transition_count": info["transition_count"],
            })

    return {"services": services}
