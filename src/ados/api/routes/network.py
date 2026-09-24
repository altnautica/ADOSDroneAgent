"""Profile-agnostic Wi-Fi client REST surface (the residual part).

Lives outside the ground-station namespace because Wi-Fi client
operations only depend on a wlan interface being present, not on the
operator's chosen profile.

The station status, the join / leave / forget / autoconnect writes and the
stable-MAC routes are served by the native front on every profile. Only the
scan remains here.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException

router = APIRouter(prefix="/v1/network", tags=["network"])


def _manager() -> Any:
    """Lazy import keeps service module loading deferred."""
    from ados.services.ground_station.wifi_client_manager import (
        get_wifi_client_manager,
    )

    return get_wifi_client_manager()


@router.get("/client/scan")
async def get_client_scan() -> dict[str, Any]:
    """Scan nearby Wi-Fi networks via nmcli.

    Returns ``{"networks": [...]}`` sorted by signal strength descending.
    """
    try:
        networks = await _manager().scan(timeout_s=10)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_SCAN_FAILED", "message": str(exc)}},
        ) from exc
    return {"networks": networks or []}


__all__ = ["router"]
