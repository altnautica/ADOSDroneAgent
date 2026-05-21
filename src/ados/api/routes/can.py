"""CAN passthrough route surface.

Reserved for a future agent-side CAN bridge. The current
implementation responds with HTTP 501 so the GCS can probe for
availability without crashing on a 404. When the bridge lands the
501 stub gets replaced with a streaming handler that opens a SocketCAN
channel against the agent's local CAN interface.

Background: most CAN access today flows end-to-end via MAVLink
passthrough between the GCS and the flight controller. The agent's
MAVLink relay forwards the relevant CAN_FRAME, CANFD_FRAME, and
CAN_FILTER_MODIFY messages plus the CAN_FORWARD command without any
message-id filter, so the existing path covers the common case.

This route exists for a future scenario where a remote DroneCAN
client (phone, headless monitoring, second SBC) needs to talk to a
CAN bus reachable only from the companion computer. The endpoint
shape will be a long-lived POST that streams encoded frames in and
back out; the body schema is intentionally not specified yet.
"""

from __future__ import annotations

from fastapi import APIRouter
from fastapi.responses import JSONResponse

router = APIRouter()


@router.post("/can/passthrough", status_code=501)
async def can_passthrough() -> JSONResponse:
    """Reserved for a future agent-side CAN passthrough bridge.

    Returns HTTP 501 with a small JSON envelope so callers can
    distinguish a deliberate not-implemented response from a missing
    route or an auth failure. The GCS treats absence of this surface
    (404 or 501) as "passthrough disabled" and falls back to the
    MAVLink CAN_FORWARD path.
    """
    return JSONResponse(
        status_code=501,
        content={
            "error": "not_implemented",
            "message": (
                "CAN passthrough planned for future agent-side support"
            ),
        },
    )
