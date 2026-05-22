"""Core setup state routes — status, finalize, skip, reset."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Request

from ados.api.deps import get_agent_app
from ados.setup import state as setup_state
from ados.setup.models import SetupStatus
from ados.setup.service import build_setup_status

from ._common import VALID_NUDGE_IDS, VALID_STEP_IDS

router = APIRouter()


@router.get("/status", response_model=SetupStatus)
async def get_setup_status(request: Request) -> SetupStatus:
    """Return the universal setup state consumed by web, CLI, and GCS clients."""
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
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


@router.post("/skip", response_model=SetupStatus)
async def skip_setup(request: Request) -> SetupStatus:
    """Mark the entire setup wizard dismissed via Skip to Home.

    Sets ``setup_skipped=true`` so the webapp's index redirect routes
    to Home immediately on the next page load. Distinct from finish:
    the operator has not actually completed the wizard, so a resume
    banner is shown on Home until they either finalize or reset.
    """
    setup_state.mark_setup_skipped()
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/step/{step_id}/skip", response_model=SetupStatus)
async def skip_setup_step(step_id: str, request: Request) -> SetupStatus:
    """Mark a step as deferred ("Skip for now")."""
    if step_id not in VALID_STEP_IDS:
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


@router.get("/nudges")
async def get_nudges() -> dict:
    """Return the set of one-shot prompt ids the operator has already
    acknowledged. The dashboard checks this on load to decide whether
    to surface each prompt; suppressing on the agent side means the
    flag follows the agent through reflashes that wipe the browser."""
    state = setup_state.read_state()
    return {"acked": sorted(state.acked_nudges)}


@router.post("/nudges/{nudge_id}/ack")
async def ack_nudge(nudge_id: str) -> dict:
    """Mark a one-shot prompt as acknowledged so it never re-renders."""
    if nudge_id not in VALID_NUDGE_IDS:
        raise HTTPException(status_code=404, detail=f"Unknown nudge id: {nudge_id}")
    state = setup_state.ack_nudge(nudge_id)
    return {"status": "ok", "acked": sorted(state.acked_nudges)}
