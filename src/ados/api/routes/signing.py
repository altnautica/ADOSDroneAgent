"""MAVLink signing enrollment and capability routes.

The agent never stores a signing key. These routes let the GCS:
  * detect whether the connected FC supports MAVLink v2 signing,
  * push a key to the FC via SETUP_SIGNING (one-shot, zeroized after),
  * clear the FC's signing store,
  * toggle SIGNING_REQUIRE on the FC,
  * read counters of signed frames observed transiting the agent.

The POST /enroll-fc body contains the raw 32-byte key as 64-char hex. The
key buffer is overwritten with zeros before the route returns. The key
MUST NEVER appear in structured logs; the log redaction helper below
strips any 64-char hex token from request bodies before they are logged.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

from ados.core.ipc import MAVLINK_SOCK, MavlinkIPCClient
from ados.core.logging import get_logger

log = get_logger("api.signing")

router = APIRouter()


def _fc_connected(app: Any) -> bool:
    """True when the router's state IPC snapshot reports a live FC link."""
    return bool(app.fc_status().connected)


def _cached_params(app: Any) -> dict[str, float]:
    """The cached param blob from the router's state IPC snapshot."""
    try:
        ipc = app.state_ipc_state() or {}
    except Exception:
        ipc = {}
    blob = ipc.get("params")
    return blob if isinstance(blob, dict) else {}


def _autopilot(app: Any) -> int:
    """MAV_AUTOPILOT id from the router's state IPC snapshot."""
    try:
        ipc = app.state_ipc_state() or {}
    except Exception:
        ipc = {}
    return int(ipc.get("autopilot", 0) or 0)


async def _connect_mavlink_ipc() -> MavlinkIPCClient:
    """Open a short-lived command-socket client. Raises on link absence."""
    ipc = MavlinkIPCClient(sock_path=MAVLINK_SOCK)
    await ipc.connect(retries=3, delay=0.25)
    return ipc


# ──────────────────────────────────────────────────────────────
# Request / response models
# ──────────────────────────────────────────────────────────────

class EnrollRequest(BaseModel):
    """Body for POST /mavlink/signing/enroll-fc.

    key_hex is the 32-byte MAVLink signing key as 64 lowercase hex chars.
    NEVER log this field. The route strips it before handing off.
    """
    key_hex: str = Field(..., min_length=64, max_length=64)
    link_id: int = Field(default=0, ge=0, le=255)
    target_system: int = Field(default=1, ge=1, le=255)
    target_component: int = Field(default=1, ge=0, le=255)


class RequireRequest(BaseModel):
    require: bool


# ──────────────────────────────────────────────────────────────
# Routes
# ──────────────────────────────────────────────────────────────

