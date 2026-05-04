"""Universal setup and onboarding API routes."""

from __future__ import annotations

from typing import Literal

from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.setup import state as setup_state
from ados.setup.hardware_check import run_hardware_check
from ados.setup.models import HardwareCheckStatus, SetupActionResult, SetupStatus
from ados.setup.profile import apply_profile
from ados.setup.service import (
    apply_cloud_choice,
    build_setup_status,
    install_cloudflare_token,
)

router = APIRouter(prefix="/v1/setup", tags=["setup"])

# Canonical step ids the wizard emits. Used to validate skip targets so
# operators cannot stash arbitrary keys in the state file.
_VALID_STEP_IDS: frozenset[str] = frozenset(
    {
        "welcome",
        "profile",
        "network",
        "hardware_check",
        "cloud_choice",
        "pair",
        "mavlink",
        "video",
        "ground_receiver",
        "remote_access",
        "finish",
    }
)


class CloudflareTokenRequest(BaseModel):
    token_or_script: str


class SelfHostedBackendRequest(BaseModel):
    url: str
    mqtt_broker: str = ""
    mqtt_port: int = 8883
    api_key: str = ""


class CloudChoiceRequest(BaseModel):
    mode: Literal["cloud", "self_hosted", "local"]
    self_hosted: SelfHostedBackendRequest | None = Field(default=None)


class ProfileChoiceRequest(BaseModel):
    profile: Literal["drone", "ground_station"]
    ground_role: Literal["direct", "relay", "receiver"] | None = Field(default=None)


@router.get("/status", response_model=SetupStatus)
async def get_setup_status(request: Request) -> SetupStatus:
    """Return the universal setup state consumed by web, CLI, and GCS clients."""
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/remote-access/cloudflare", response_model=SetupActionResult)
async def configure_cloudflare_tunnel(request: CloudflareTokenRequest) -> SetupActionResult:
    """Install a remotely managed Cloudflare Tunnel token or install command."""
    return install_cloudflare_token(get_agent_app(), request.token_or_script)


@router.post("/profile", response_model=SetupActionResult)
async def configure_profile(request: ProfileChoiceRequest) -> SetupActionResult:
    """Persist the operator's profile choice from the onboarding wizard.

    ``ground_role`` is required when ``profile`` is ``ground_station``
    and selects the distributed-RX role on the ground station node.
    """
    return apply_profile(
        get_agent_app(),
        profile=request.profile,
        ground_role=request.ground_role,
    )


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
    """Re-run the hardware sweep on demand (no caching).

    Wired so the wizard can offer a Refresh button after the operator
    hot-plugs a USB device or swaps a camera mid-onboarding.
    """
    runtime = get_agent_app()
    config = runtime.config
    profile = str(config.agent.profile)
    if profile == "auto":
        profile = "drone"
    role = str(getattr(config.ground_station, "role", "direct") or "direct")
    return run_hardware_check(runtime, profile=profile, ground_role=role)


@router.post("/cloud-choice", response_model=SetupActionResult)
async def configure_cloud_choice(request: CloudChoiceRequest) -> SetupActionResult:
    """Set the agent's cloud posture (cloud / self_hosted / local).

    Local mode disables the cloud relay entirely. Self-hosted mode records
    the operator's Convex + MQTT coordinates and writes any provided API
    key to a root-owned secret file. The API key is never echoed back.
    """
    self_hosted = request.self_hosted.model_dump() if request.self_hosted else None
    return apply_cloud_choice(
        get_agent_app(),
        mode=request.mode,
        self_hosted=self_hosted,
    )


@router.post("/finish", response_model=SetupStatus)
async def finalize_setup(request: Request) -> SetupStatus:
    """Mark the onboarding wizard complete.

    Sets ``setup_finalized=true`` in persistent state. The universal
    webapp uses this flag to gate the rest of the app surface.
    """
    setup_state.mark_finalized()
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/step/{step_id}/skip", response_model=SetupStatus)
async def skip_setup_step(step_id: str, request: Request) -> SetupStatus:
    """Mark a step as deferred ("Skip for now")."""
    if step_id not in _VALID_STEP_IDS:
        raise HTTPException(status_code=404, detail=f"Unknown step id: {step_id}")
    if step_id in {"welcome", "finish"}:
        raise HTTPException(status_code=400, detail=f"Step '{step_id}' cannot be skipped")
    setup_state.mark_skipped(step_id)
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/reset", response_model=SetupStatus)
async def reset_setup(request: Request) -> SetupStatus:
    """Clear setup_finalized and the skipped-step set.

    Used by the Setup page's "Re-run setup" action so the wizard
    re-engages the operator with the full step list.
    """
    setup_state.reset_state()
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )
