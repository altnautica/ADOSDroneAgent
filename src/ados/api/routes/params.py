"""FC parameter routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/params")
async def get_all_params():
    """All cached FC parameters."""
    app = get_agent_app()
    if app._vehicle_state:
        return {
            "params": app._vehicle_state.params,
            "count": app._vehicle_state.param_count,
            "cached": len(app._vehicle_state.params),
        }
    return {"params": {}, "count": 0, "cached": 0}


@router.get("/params/{name}")
async def get_param(name: str):
    """Get a single parameter by name."""
    app = get_agent_app()
    if app._vehicle_state and name in app._vehicle_state.params:
        return {"name": name, "value": app._vehicle_state.params[name]}
    raise HTTPException(status_code=404, detail=f"Parameter '{name}' not found")
