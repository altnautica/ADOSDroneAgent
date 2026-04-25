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
    EthernetConfigUpdate,
    ModemConfigUpdate,
    ShareUplinkUpdate,
    UplinkPriorityUpdate,
    WifiJoinRequest,
)


router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.get("/network")
async def get_ground_station_network() -> dict[str, Any]:
    """Network uplinks view.

    Covers all four uplinks (wifi_client, ethernet, modem_4g) plus the
    active_uplink + priority surfaced by UplinkRouter and the
    share_uplink flag.
    """
    app = _gs._require_ground_profile()
    router_view = _gs._router_state_view()
    return {
        "ap": _gs._ap_view(app),
        "wifi_client": await _gs._wifi_client_view(),
        "ethernet": await _gs._ethernet_view(),
        "modem_4g": await _gs._modem_view(),
        "active_uplink": router_view["active_uplink"],
        "priority": router_view["priority"],
        "share_uplink": _gs._load_share_uplink_flag(),
    }


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


@router.get("/network/ethernet")
async def get_network_ethernet() -> dict[str, Any]:
    """Return the configured Ethernet profile plus live link state."""
    _gs._require_ground_profile()
    try:
        return await _gs._ethernet_mgr().config()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_ETHERNET_CONFIG_READ_FAILED", "message": str(exc)}},
        ) from exc


@router.put("/network/ethernet")
async def put_network_ethernet(update: EthernetConfigUpdate) -> dict[str, Any]:
    """Apply Ethernet IPv4 config. mode=dhcp or mode=static."""
    _gs._require_ground_profile()
    mgr = _gs._ethernet_mgr()

    if update.mode == "static":
        if not update.ip or not update.gateway:
            raise HTTPException(
                status_code=400,
                detail={
                    "error": {
                        "code": "E_ETHERNET_STATIC_MISSING_FIELDS",
                        "message": "ip and gateway are required when mode=static",
                    }
                },
            )
        try:
            result = await mgr.configure_static(
                ip=update.ip,
                gateway=update.gateway,
                dns=list(update.dns or []),
            )
        except Exception as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": {"code": "E_ETHERNET_STATIC_FAILED", "message": str(exc)}},
            ) from exc
    else:
        try:
            result = await mgr.configure_dhcp()
        except Exception as exc:
            raise HTTPException(
                status_code=500,
                detail={"error": {"code": "E_ETHERNET_DHCP_FAILED", "message": str(exc)}},
            ) from exc

    if isinstance(result, dict) and result.get("ok") is False:
        err_code = (
            "E_ETHERNET_NO_CONNECTION"
            if result.get("error") == "no_ethernet_connection"
            else "E_ETHERNET_APPLY_FAILED"
        )
        raise HTTPException(
            status_code=500,
            detail={
                "error": {
                    "code": err_code,
                    "message": str(result.get("error") or "ethernet_apply_failed"),
                    "hint": result.get("hint"),
                }
            },
        )

    try:
        return await mgr.config()
    except Exception:
        return {"mode": update.mode, "applied": True}


@router.get("/network/client/scan")
async def get_network_client_scan() -> dict[str, Any]:
    """Scan for nearby WiFi networks via nmcli."""
    _gs._require_ground_profile()
    try:
        networks = await _gs._wifi_client_manager().scan(timeout_s=10)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_SCAN_FAILED", "message": str(exc)}},
        ) from exc
    return {"networks": networks or []}


@router.put("/network/client/join")
async def put_network_client_join(req: WifiJoinRequest) -> dict[str, Any]:
    """Join a WiFi network. 409 on AP mutex conflict without force."""
    _gs._require_ground_profile()

    try:
        result = await _gs._wifi_client_manager().join(
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
        "joined": bool(result.get("joined", False)) if isinstance(result, dict) else False,
        "ip": result.get("ip") if isinstance(result, dict) else None,
        "gateway": result.get("gateway") if isinstance(result, dict) else None,
        "error": result.get("error") if isinstance(result, dict) else None,
    }


@router.delete("/network/client")
async def delete_network_client() -> dict[str, Any]:
    """Disconnect the current WiFi client connection."""
    _gs._require_ground_profile()
    try:
        return await _gs._wifi_client_manager().leave()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_LEAVE_FAILED", "message": str(exc)}},
        ) from exc


@router.get("/network/modem")
async def get_network_modem() -> dict[str, Any]:
    """Return modem status + data usage + configured cap."""
    _gs._require_ground_profile()
    return await _gs._modem_view()


@router.put("/network/modem")
async def put_network_modem(update: ModemConfigUpdate) -> dict[str, Any]:
    """Update modem config (apn, cap_gb, enabled). Returns refreshed view."""
    _gs._require_ground_profile()
    try:
        await _gs._modem_mgr().configure(
            apn=update.apn,
            cap_gb=update.cap_gb,
            enabled=update.enabled,
        )
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_MODEM_CONFIGURE_FAILED", "message": str(exc)}},
        ) from exc
    return await _gs._modem_view()


@router.get("/network/priority")
async def get_network_priority() -> dict[str, Any]:
    """Return the current uplink priority list."""
    _gs._require_ground_profile()
    try:
        priority = list(_gs._uplink_router().get_priority())
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UPLINK_PRIORITY_FAILED", "message": str(exc)}},
        ) from exc
    return {"priority": priority}


@router.put("/network/priority")
async def put_network_priority(update: UplinkPriorityUpdate) -> dict[str, Any]:
    """Set the uplink priority list. Router persists to its own JSON."""
    _gs._require_ground_profile()
    try:
        _gs._uplink_router().set_priority(list(update.priority))
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_UPLINK_PRIORITY_INVALID", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UPLINK_PRIORITY_FAILED", "message": str(exc)}},
        ) from exc
    return {"priority": list(_gs._uplink_router().get_priority())}


@router.put("/network/share_uplink")
async def put_network_share_uplink(update: ShareUplinkUpdate) -> dict[str, Any]:
    """Toggle IPv4 forwarding + NAT masquerade for AP clients.

    POC implementation: writes net.ipv4.ip_forward via sysctl and adds
    a MASQUERADE rule on the active uplink. On failure the flag is
    still persisted and the error is surfaced in the response. Full
    firewall management comes in a later phase.
    """
    _gs._require_ground_profile()
    try:
        _gs._persist_share_uplink_flag(update.enabled)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    applied = await _gs._apply_share_uplink(bool(update.enabled))
    return {
        "enabled": bool(update.enabled),
        "applied": applied["applied"],
        "apply_error": applied["apply_error"],
        "backend": applied.get("backend"),
    }
