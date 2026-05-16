"""Hardware-check routes."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.core.profile import _read_profile_conf_value
from ados.setup import hardware_state
from ados.setup.hardware_check import (
    run_hardware_check,
    run_hardware_check_fresh,
)
from ados.setup.models import HardwareCheckStatus

router = APIRouter()


def _resolve_profile(config) -> tuple[str, str]:
    """Pick the effective (profile, ground_role) for the hardware sweep.

    Order: explicit config.agent.profile → /etc/ados/profile.conf →
    "drone" as a final fallback. Without the profile.conf consultation
    the sweep silently reports drone-side items on a ground station
    that hasn't finalized the setup wizard yet.
    """
    raw = str(getattr(getattr(config, "agent", None), "profile", "") or "")
    profile = raw if raw in ("drone", "ground_station") else ""
    if not profile:
        conf_raw = _read_profile_conf_value()
        if conf_raw == "drone":
            profile = "drone"
        elif conf_raw in ("ground_station", "ground-station"):
            profile = "ground_station"
    if not profile:
        profile = "drone"
    role = str(getattr(getattr(config, "ground_station", None), "role", "direct") or "direct")
    return profile, role


@router.get("/hardware-check", response_model=HardwareCheckStatus)
async def get_hardware_check() -> HardwareCheckStatus:
    """Return the per-component hardware readiness snapshot for the active profile."""
    runtime = get_agent_app()
    profile, role = _resolve_profile(runtime.config)
    return run_hardware_check(runtime, profile=profile, ground_role=role)


@router.post("/hardware-check/refresh", response_model=HardwareCheckStatus)
async def refresh_hardware_check() -> HardwareCheckStatus:
    """Re-run the hardware sweep on demand and persist the snapshot.

    Wired so the wizard can offer a Rescan button after the operator
    hot-plugs a USB device or swaps a camera mid-onboarding. Bypasses
    the read-path cache and always writes a fresh snapshot.
    """
    runtime = get_agent_app()
    profile, role = _resolve_profile(runtime.config)
    fresh = run_hardware_check_fresh(runtime, profile=profile, ground_role=role)
    hardware_state.write(fresh)
    return fresh
