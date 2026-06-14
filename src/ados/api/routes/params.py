"""FC parameter routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger

log = get_logger("api.params")

router = APIRouter()


class ParamSetRequest(BaseModel):
    """Body for ``POST /api/params/{name}``."""

    value: float = Field(..., description="New numeric value to write to the FC.")


class ParamSetResponse(BaseModel):
    name: str
    value: float
    ack: bool
    cached_value: float | None = None
    message: str = ""


def _resolve_priming_flags(app) -> dict:
    """Resolve param-sweep flags, preferring the state-IPC snapshot.

    Under the multi-process supervisor the FC connection lives in the
    mavlink-service process. ``app.fc_connection()`` returns ``None``
    on the API process, so ``getattr(fc, "param_priming", False)``
    silently coerces to False and the timeout never surfaces. The
    mavlink-service publishes the flags through ``/run/ados/state.sock``;
    we read them here first and only fall back to a local FCConnection
    handle for single-process / test runs.
    """
    try:
        ipc = app.state_ipc_state() or {}
    except Exception:
        ipc = {}
    if "param_priming" in ipc:
        return {
            "priming": bool(ipc.get("param_priming", False)),
            "priming_timeout": bool(ipc.get("param_sweep_timed_out", False)),
            "priming_send_failed": bool(ipc.get("param_sweep_send_failed", False)),
        }
    fc = app.fc_connection()
    return {
        "priming": bool(getattr(fc, "param_priming", False)),
        "priming_timeout": bool(getattr(fc, "param_sweep_timed_out", False)),
        "priming_send_failed": bool(getattr(fc, "param_sweep_send_failed", False)),
    }


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

