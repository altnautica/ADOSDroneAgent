"""Network uplink endpoints.

Covers the AP, ethernet, wifi-client, modem, priority list, and the
share-uplink toggle. Read-only views are aggregated under GET /network.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException

from ados.api.routes import ground_station as _gs
from ados.api.routes.ground_station._common import (
    ApUpdate,
    WifiJoinRequest,
)

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.put("/network/ap")
async def put_ground_station_ap(update: ApUpdate) -> dict[str, Any]:
    """Apply AP config change via HostapdManager.apply_ap_config()."""
    app = _gs._require_ground_profile()

    mgr = _gs._hostapd_manager(app)
    try:
        ok = mgr.apply_ap_config(
            ssid=update.ssid,
            passphrase=update.passphrase,
            channel=update.channel,
        )
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_AP_APPLY_FAILED", "message": str(exc)}},
        ) from exc

    if not ok:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_AP_APPLY_FAILED"}},
        )

    # `enabled` is a hint. When False we stop; when True and not running
    # yet we start. Unchanged enabled leaves the unit alone.
    if update.enabled is not None:
        try:
            running = mgr.status().get("running", False)
            if update.enabled and not running:
                mgr.start()
            elif not update.enabled and running:
                mgr.stop()
        except Exception:
            pass

    # Persist channel / SSID back to agent config for reboot survival.
    hotspot = getattr(app.config.network, "hotspot", None)
    if hotspot is not None:
        if update.channel is not None and hasattr(hotspot, "channel"):
            setattr(hotspot, "channel", update.channel)
        if update.ssid is not None and hasattr(hotspot, "ssid"):
            setattr(hotspot, "ssid", update.ssid)
        _gs._save_config(app)

    return _gs._ap_view(app)


@router.put("/network/client/join")
async def put_network_client_join(req: WifiJoinRequest) -> dict[str, Any]:
    """Join a WiFi network. 409 on AP mutex conflict without force.

    Forwards to the native uplink daemon's command socket when it owns net so
    the REST process never drives nmcli on wlan0 and races the daemon's WiFi
    manager for the radio.
    """
    _gs._require_ground_profile()

    from ados.core.runtime_mode import is_service_native

    async def _join_via_manager() -> dict[str, Any]:
        try:
            return await _gs._wifi_client_manager().join(
                ssid=req.ssid,
                passphrase=req.passphrase,
                force=bool(req.force),
            )
        except Exception as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": {"code": "E_WIFI_JOIN_FAILED", "message": str(exc)}},
            ) from exc

    if is_service_native("net"):
        from ados.services.network import wifi_cmd_client

        try:
            result = await wifi_cmd_client.join(req.ssid, req.passphrase, bool(req.force))
        except wifi_cmd_client.NetCmdUnavailableError:
            result = await _join_via_manager()
        except wifi_cmd_client.NetCmdError as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": {"code": "E_WIFI_JOIN_FAILED", "message": str(exc)}},
            ) from exc
    else:
        result = await _join_via_manager()

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
        "joined": bool(result.get("joined", False)) if isinstance(result, dict) else False,
        "ip": result.get("ip") if isinstance(result, dict) else None,
        "gateway": result.get("gateway") if isinstance(result, dict) else None,
        "error": result.get("error") if isinstance(result, dict) else None,
    }


@router.delete("/network/client")
async def delete_network_client() -> dict[str, Any]:
    """Disconnect the current WiFi client connection.

    Forwards to the native uplink daemon's command socket when it owns net.
    """
    _gs._require_ground_profile()

    from ados.core.runtime_mode import is_service_native

    if is_service_native("net"):
        from ados.services.network import wifi_cmd_client

        try:
            return await wifi_cmd_client.leave()
        except wifi_cmd_client.NetCmdUnavailableError:
            pass  # native flag set but socket down → packaged fallback below
        except wifi_cmd_client.NetCmdError as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": {"code": "E_WIFI_LEAVE_FAILED", "message": str(exc)}},
            ) from exc
    try:
        return await _gs._wifi_client_manager().leave()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_LEAVE_FAILED", "message": str(exc)}},
        ) from exc
