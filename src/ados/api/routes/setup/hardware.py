"""Hardware-check routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.setup import hardware_state
from ados.setup.hardware_check import (
    run_hardware_check,
    run_hardware_check_fresh,
)
from ados.setup.models import HardwareCheckStatus

router = APIRouter()


@router.get("/hardware-check", response_model=HardwareCheckStatus)
async def get_hardware_check() -> HardwareCheckStatus:
    """Return the per-component hardware readiness snapshot for the active profile."""
    runtime = get_agent_app()
    config = runtime.config
    profile = str(config.agent.profile)
    if profile == "auto":
        profile = "drone"
    role = str(getattr(config.ground_station, "role", "direct") or "direct")
    return run_hardware_check(runtime, profile=profile, ground_role=role)


@router.post("/hardware-check/refresh", response_model=HardwareCheckStatus)
async def refresh_hardware_check() -> HardwareCheckStatus:
    """Re-run the hardware sweep on demand and persist the snapshot.

    Wired so the wizard can offer a Rescan button after the operator
    hot-plugs a USB device or swaps a camera mid-onboarding. Bypasses
    the read-path cache and always writes a fresh snapshot.
    """
    runtime = get_agent_app()
    config = runtime.config
    profile = str(config.agent.profile)
    if profile == "auto":
        profile = "drone"
    role = str(getattr(config.ground_station, "role", "direct") or "direct")
    fresh = run_hardware_check_fresh(runtime, profile=profile, ground_role=role)
    hardware_state.write(fresh)
    return fresh
