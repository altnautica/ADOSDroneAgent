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

