"""Unified one-shot ticket mint for WebSocket auth.

Browsers cannot set ``X-ADOS-Key`` on a WebSocket handshake, so the
GCS first exchanges its pairing key (enforced on the REST middleware)
for a short-lived random ticket and hands the ticket to
``new WebSocket(url, ["ados-ws-ticket", <ticket>])``. The ticket is
bound to a specific ``scope`` string so a ticket minted for one route
cannot be replayed against another.

The legacy plugin-specific ticket endpoint at
``POST /api/plugins/jobs/{job_id}/ticket`` still works and shares the
same underlying ticket store.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.middleware.ws_auth import ws_ticket_store

router = APIRouter(prefix="/_ws", tags=["ws-auth"])


# Allowed scopes the generic endpoint will mint tickets for. The agent
# is the issuer and the validator; pinning the set to the routes it
# knows about prevents a stray client from minting tickets for scopes
# the agent will never check. Each entry maps a public ``scope`` value
# to a short human-readable label used in error messages.
ALLOWED_SCOPES: frozenset[str] = frozenset(
    {
        "setup.cloudflare_logs",
        "gs.pic_events",
        "gs.mavlink_ws",
        "gs.uplink_events",
        "gs.mesh_events",
    }
)


class TicketRequest(BaseModel):
    scope: str = Field(
        ...,
        description="Logical scope this ticket will be consumed by. Must be one of the agent's known WebSocket routes.",
    )
    ttl_seconds: int | None = Field(
        default=None,
        ge=1,
        le=120,
        description="Override for the default 30 s lifetime. Capped at 120 s.",
    )


class TicketResponse(BaseModel):
    ok: bool
    ticket: str
    scope: str
    expires_at: int


@router.post("/ticket", response_model=TicketResponse)
async def mint_ws_ticket(req: TicketRequest) -> TicketResponse:
    """Mint a one-shot ticket for the named WebSocket scope.

    Authenticated by ``X-ADOS-Key`` via the REST middleware. Ticket
    lifetime is 30 s by default and the same ticket cannot be consumed
    twice.
    """
    if req.scope not in ALLOWED_SCOPES:
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_UNKNOWN_SCOPE",
                    "message": f"scope '{req.scope}' is not a known WebSocket route",
                }
            },
        )
    ttl = req.ttl_seconds or ws_ticket_store.DEFAULT_TTL_SECONDS
    ticket, expires_at = await ws_ticket_store.issue(req.scope, ttl_seconds=ttl)
    return TicketResponse(
        ok=True,
        ticket=ticket,
        scope=req.scope,
        expires_at=expires_at,
    )
