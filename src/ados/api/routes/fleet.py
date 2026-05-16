"""Fleet / MeshNet routes.

Fleet awareness is opt-in: until the device enrolls in a fleet,
``/fleet/enrollment`` reports ``enrolled: False`` and ``/fleet/peers``
returns an empty list. The empty list is the canonical "no peers yet"
response, not a placeholder — callers that want richer fleet state
should look at the cloud-relay heartbeat instead.
"""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/fleet/enrollment")
async def get_enrollment():
    """Get fleet enrollment status for this device."""
    return {"enrolled": False}


@router.get("/fleet/peers")
async def list_peers():
    """List peers discovered for this device's fleet.

    Empty list = no peers known yet; with enrollment off, this is the
    expected steady-state response.
    """
    return []
