"""Profile-selection route."""

from __future__ import annotations

import asyncio

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.setup.models import ProfileSuggestion, SetupActionResult
from ados.setup.profile import apply_profile, redetect_profile_suggestion

from ._models import ProfileChoiceRequest

router = APIRouter()


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
        auto_restart=request.auto_restart,
    )


@router.post("/profile/redetect", response_model=ProfileSuggestion)
async def redetect_profile() -> ProfileSuggestion:
    """Re-run the hardware probes, replace the stored snapshot, return it.

    The setup status serves the stored detection; this is the explicit way
    to refresh it after hardware changed.
    """
    config = await get_agent_app().aconfig()
    return await asyncio.to_thread(redetect_profile_suggestion, config)
