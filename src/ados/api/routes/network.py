"""Profile-agnostic Wi-Fi client REST surface.

Lives outside the ground-station namespace because Wi-Fi client
operations (scan / join / leave / status / saved-connection management)
only depend on a wlan interface being present, not on the operator's
chosen profile. Drones and ground stations both reach NetworkManager
the same way; gating these routes to one profile artificially blocks
drone agents from joining a bench network from the dashboard.

The handlers reuse the singleton ``WifiClientManager`` exported from
``ados.services.ground_station.wifi_client_manager`` so the underlying
nmcli logic and event bus stay in one place.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException

from ados.api.routes.ground_station._common.models import WifiJoinRequest

router = APIRouter(prefix="/v1/network", tags=["network"])


def _manager() -> Any:
    """Lazy import keeps service module loading deferred."""
    from ados.services.ground_station.wifi_client_manager import (
        get_wifi_client_manager,
    )

    return get_wifi_client_manager()


@router.get("/client/status")
async def get_client_status() -> dict[str, Any]:
    """Current Wi-Fi client connection state.

    Returns ``{connected, ssid, bssid, signal, ip, gateway, security}``
    from ``WifiClientManager.status()``.
    """
    try:
        return await _manager().status()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_STATUS_FAILED", "message": str(exc)}},
        ) from exc


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


@router.get("/client/configured")
async def get_client_configured() -> dict[str, Any]:
    """List saved NetworkManager Wi-Fi profiles."""
    try:
        connections = await _manager().configured_connections()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={
                "error": {"code": "E_WIFI_CONFIGURED_FAILED", "message": str(exc)},
            },
        ) from exc
    return {"connections": connections or []}


@router.put("/client/join")
async def put_client_join(req: WifiJoinRequest) -> dict[str, Any]:
    """Join a Wi-Fi network. 409 on AP mutex conflict without force."""
    try:
        result = await _manager().join(
            ssid=req.ssid,
            passphrase=req.passphrase,
            force=bool(req.force),
        )
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_JOIN_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict) and not result.get("joined"):
        err = str(result.get("error") or "")
        if err == "wlan0_busy_ap_active":
            raise HTTPException(
                status_code=409,
                detail={
                    "error": {
                        "code": "E_WLAN0_BUSY_AP_ACTIVE",
                        "message": result.get("hint")
                        or "AP is active; retry with force=true to steal wlan0",
                    },
                    "needs_force": True,
                },
            )

    return {
        "joined": bool(result.get("joined", False))
        if isinstance(result, dict)
        else False,
        "ip": result.get("ip") if isinstance(result, dict) else None,
        "gateway": result.get("gateway") if isinstance(result, dict) else None,
        "error": result.get("error") if isinstance(result, dict) else None,
    }


@router.delete("/client")
async def delete_client() -> dict[str, Any]:
    """Disconnect the current Wi-Fi client link."""
    try:
        return await _manager().leave()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_LEAVE_FAILED", "message": str(exc)}},
        ) from exc


@router.delete("/client/configured/{name}")
async def delete_client_configured(name: str) -> dict[str, Any]:
    """Forget a saved NetworkManager Wi-Fi profile by name."""
    try:
        result = await _manager().forget(name)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_FORGET_FAILED", "message": str(exc)}},
        ) from exc
    if isinstance(result, dict) and not result.get("forgot"):
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_WIFI_FORGET_FAILED",
                    "message": str(result.get("error") or "nmcli_failed"),
                },
            },
        )
    return result


@router.put("/client/configured/{name}/autoconnect")
async def put_client_autoconnect(
    name: str, body: dict[str, Any]
) -> dict[str, Any]:
    """Toggle the autoconnect flag on a saved Wi-Fi profile."""
    enabled = bool(body.get("enabled"))
    try:
        result = await _manager().set_autoconnect(name, enabled)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={
                "error": {
                    "code": "E_WIFI_AUTOCONNECT_FAILED",
                    "message": str(exc),
                },
            },
        ) from exc
    if isinstance(result, dict) and result.get("error"):
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_WIFI_AUTOCONNECT_FAILED",
                    "message": str(result.get("error")),
                },
            },
        )
    return result


__all__ = ["router"]
