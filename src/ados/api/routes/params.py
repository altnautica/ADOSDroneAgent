"""FC parameter routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/params")
async def get_all_params():
    """All cached FC parameters, served from ParamCache when available."""
    app = get_agent_app()
    param_cache = app.param_cache()
    vehicle_state = app.vehicle_state()

    # Prefer ParamCache (persistent) if available
    if param_cache is not None:
        all_params = param_cache.get_all()
        return {
            "params": all_params,
            "count": vehicle_state.param_count if vehicle_state else len(all_params),
            "cached": len(all_params),
        }

    # Fall back to VehicleState in-memory params
    if vehicle_state:
        return {
            "params": vehicle_state.params,
            "count": vehicle_state.param_count,
            "cached": len(vehicle_state.params),
        }
    return {"params": {}, "count": 0, "cached": 0}


@router.get("/params/{name}")
async def get_param(name: str):
    """Get a single parameter by name."""
    app = get_agent_app()
    param_cache = app.param_cache()
    vehicle_state = app.vehicle_state()

    # Try ParamCache first
    if param_cache is not None:
        value = param_cache.get(name)
        if value is not None:
            return {"name": name, "value": value}

    # Fall back to VehicleState
    if vehicle_state and name in vehicle_state.params:
        return {"name": name, "value": vehicle_state.params[name]}

    raise HTTPException(status_code=404, detail=f"Parameter '{name}' not found")
