"""FC parameter routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/params")
async def get_all_params():
    """All cached FC parameters, served from ParamCache when available."""
    app = get_agent_app()

    # Prefer ParamCache (persistent) if available
    if app._param_cache is not None:
        all_params = app._param_cache.get_all()
        return {
            "params": all_params,
            "count": app._vehicle_state.param_count if app._vehicle_state else len(all_params),
            "cached": len(all_params),
        }

    # Fall back to VehicleState in-memory params
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

    # Try ParamCache first
    if app._param_cache is not None:
        value = app._param_cache.get(name)
        if value is not None:
            return {"name": name, "value": value}

    # Fall back to VehicleState
    if app._vehicle_state and name in app._vehicle_state.params:
        return {"name": name, "value": app._vehicle_state.params[name]}

    raise HTTPException(status_code=404, detail=f"Parameter '{name}' not found")
