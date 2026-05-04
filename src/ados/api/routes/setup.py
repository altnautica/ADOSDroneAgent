"""Universal setup and onboarding API routes."""

from __future__ import annotations

from fastapi import APIRouter, Request
from pydantic import BaseModel

from ados.api.deps import get_agent_app
from ados.setup.models import SetupActionResult, SetupStatus
from ados.setup.service import build_setup_status, install_cloudflare_token

router = APIRouter(prefix="/v1/setup", tags=["setup"])


class CloudflareTokenRequest(BaseModel):
    token_or_script: str


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
