"""Profile-selection route."""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.setup.models import SetupActionResult
from ados.setup.profile import apply_profile

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
