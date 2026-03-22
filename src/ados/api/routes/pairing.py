"""Pairing API routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter(tags=["pairing"])


class ClaimRequest(BaseModel):
    user_id: str


class ClaimResponse(BaseModel):
    api_key: str
    device_id: str
    name: str
    mdns_host: str


class PairingInfo(BaseModel):
    device_id: str
    name: str
    version: str
    board: str
    paired: bool
    pairing_code: str | None = None
    owner_id: str | None = None
    paired_at: float | None = None
    mdns_host: str


@router.get("/pairing/info", response_model=PairingInfo)
async def get_pairing_info():
    """Get pairing info. No auth required."""
    app = get_agent_app()
    pm = app.pairing_manager
    info = pm.get_info()
    discovery = app.discovery_service
    short_id = app.config.agent.device_id[:6].lower()

    return PairingInfo(
        device_id=app.config.agent.device_id,
        name=app.config.agent.name,
        version=__version__,
        board=app.board_name,
        paired=info["paired"],
        pairing_code=info.get("pairing_code"),
        owner_id=info.get("owner_id"),
        paired_at=info.get("paired_at"),
        mdns_host=discovery.mdns_hostname if discovery else f"ados-{short_id}.local",
    )


@router.get("/pairing/code")
async def get_pairing_code():
    """Get just the pairing code. No auth required."""
    app = get_agent_app()
    pm = app.pairing_manager
    if pm.is_paired:
        raise HTTPException(status_code=409, detail="Already paired")
    return {"code": pm.get_or_create_code()}


@router.post("/pairing/claim", response_model=ClaimResponse)
async def claim_pairing(request: ClaimRequest):
    """Claim this agent for a user (local pairing). No auth required, only works when unpaired."""
    app = get_agent_app()
    pm = app.pairing_manager
    if pm.is_paired:
        raise HTTPException(status_code=409, detail="Already paired. Unpair first.")

    api_key = pm.claim(request.user_id)
    discovery = app.discovery_service
    short_id = app.config.agent.device_id[:6].lower()

    # Update mDNS TXT records
    if discovery:
        await discovery.update_txt(paired=True, owner=request.user_id)

    return ClaimResponse(
        api_key=api_key,
        device_id=app.config.agent.device_id,
        name=app.config.agent.name,
        mdns_host=discovery.mdns_hostname if discovery else f"ados-{short_id}.local",
    )


@router.post("/pairing/unpair")
async def unpair():
    """Unpair this agent. Requires valid API key (enforced by auth middleware)."""
    app = get_agent_app()
    pm = app.pairing_manager
    if not pm.is_paired:
        raise HTTPException(status_code=409, detail="Not paired")

    pm.unpair()
    discovery = app.discovery_service

    # Update mDNS with new code
    new_code = pm.get_or_create_code()
    if discovery:
        await discovery.update_txt(paired=False, code=new_code)

    return {"status": "unpaired", "new_code": new_code}
