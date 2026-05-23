"""FC parameter routes."""

from __future__ import annotations

import asyncio
import math

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


@router.get("/params")
async def get_all_params():
    """All cached FC parameters, served from ParamCache when available.

    The response carries a ``priming`` flag and a ``progress`` block so
    the Telemetry page can render an in-flight progress bar between the
    PARAM_REQUEST_LIST sweep firing and the cache catching up to the
    FC's advertised total. ``priming_timeout`` flips true when the FC
    stayed silent past the sweep deadline; ``priming_send_failed`` flips
    true when the PARAM_REQUEST_LIST send itself raised at the link
    layer. The dashboard reads these to swap the spinner for an
    actionable empty state instead of looping forever.
    """
    app = get_agent_app()
    param_cache = app.param_cache()
    vehicle_state = app.vehicle_state()
    fc = app.fc_connection()
    flags = _resolve_priming_flags(app)

    expected = vehicle_state.param_count if vehicle_state else 0
    if param_cache is not None:
        all_params = param_cache.get_all()
        cached = len(all_params)
        if fc is not None:
            try:
                fc.note_param_progress(cached, expected)
            except AttributeError:
                pass
        return {
            "params": all_params,
            "count": expected or cached,
            "cached": cached,
            **flags,
            "progress": {"got": cached, "expected": expected},
        }

    if vehicle_state:
        cached = len(vehicle_state.params)
        if fc is not None:
            try:
                fc.note_param_progress(cached, vehicle_state.param_count)
            except AttributeError:
                pass
        return {
            "params": vehicle_state.params,
            "count": vehicle_state.param_count,
            "cached": cached,
            **flags,
            "progress": {"got": cached, "expected": vehicle_state.param_count},
        }

    # No in-process vehicle state, no param cache — fall back entirely
    # to the IPC snapshot for cached/expected counts too. This is the
    # production path on the multi-process supervisor.
    try:
        ipc = app.state_ipc_state() or {}
    except Exception:
        ipc = {}
    cached = int(ipc.get("param_cached_count", 0) or 0)
    expected = int(ipc.get("param_expected_count", 0) or 0)
    ipc_params = ipc.get("params") if isinstance(ipc.get("params"), dict) else {}
    return {
        "params": ipc_params,
        "count": expected or cached,
        "cached": cached,
        **flags,
        "progress": {"got": cached, "expected": expected},
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


@router.post("/params/{name}", response_model=ParamSetResponse)
async def set_param(name: str, request: ParamSetRequest) -> ParamSetResponse:
    """Write a parameter to the FC.

    The endpoint refuses to write parameters the agent has never seen
    (i.e., not present in ParamCache or VehicleState). This guards against
    typos that would push garbage parameters into the FC.

    The write is fire-and-forget at the MAVLink level: the FC echoes
    back PARAM_VALUE asynchronously, the inbound stream updates the
    cache, and we poll the cache for up to 2 seconds to confirm the
    new value landed.
    """
    if not math.isfinite(request.value):
        raise HTTPException(
            status_code=400, detail="value must be a finite number"
        )

    app = get_agent_app()
    param_cache = app.param_cache()
    vehicle_state = app.vehicle_state()
    fc = app.fc_connection()

    # Confirm the parameter is known (refuse writes to unknown params)
    known_type: int | None = None
    if param_cache is not None:
        entry = param_cache._params.get(name)  # noqa: SLF001 — internal access for type
        if entry is not None:
            known_type = entry.param_type
    if known_type is None and vehicle_state and name in vehicle_state.params:
        # We have a value but no type metadata. ArduPilot accepts a 0
        # type and infers from the canonical type table.
        known_type = 0
    if known_type is None:
        raise HTTPException(
            status_code=404,
            detail=f"Parameter '{name}' not in cache; agent must observe a "
                   "PARAM_VALUE for it before writes are allowed",
        )

    if fc is None or not getattr(fc, "connected", False):
        raise HTTPException(status_code=503, detail="FC not connected")

    conn = getattr(fc, "connection", None)
    if conn is None:
        raise HTTPException(status_code=503, detail="FC connection unavailable")

    # Send the PARAM_SET. ArduPilot saves to EEPROM on receipt.
    try:
        conn.mav.param_set_send(
            getattr(fc, "target_system", 1) or 1,
            getattr(fc, "target_component", 1) or 1,
            name.encode("ascii"),
            float(request.value),
            int(known_type),
        )
    except Exception as exc:  # noqa: BLE001 (pymavlink can raise broad)
        log.warning("param_set_send_failed", name=name, error=str(exc))
        raise HTTPException(
            status_code=500, detail=f"Failed to send PARAM_SET: {exc}"
        ) from exc

    # Poll the cache for up to 2s to confirm the FC echoed back
    # the new value. The streams service updates the cache as
    # PARAM_VALUE messages arrive.
    target = float(request.value)
    deadline = asyncio.get_event_loop().time() + 2.0
    cached_value: float | None = None
    ack = False
    while asyncio.get_event_loop().time() < deadline:
        if param_cache is not None:
            cached_value = param_cache.get(name)
        elif vehicle_state and name in vehicle_state.params:
            cached_value = vehicle_state.params[name]
        if cached_value is not None and abs(cached_value - target) < 1e-6:
            ack = True
            break
        await asyncio.sleep(0.1)

    log.info(
        "param_set",
        name=name,
        value=target,
        ack=ack,
        cached_value=cached_value,
    )
    return ParamSetResponse(
        name=name,
        value=target,
        ack=ack,
        cached_value=cached_value,
        message="" if ack else "FC did not echo PARAM_VALUE within 2s",
    )
