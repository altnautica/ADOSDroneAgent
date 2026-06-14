"""Ground-station /status endpoint.

Returns the OLED-aligned snapshot used by the ground node UI and the
GCS Hardware tab.
"""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])

