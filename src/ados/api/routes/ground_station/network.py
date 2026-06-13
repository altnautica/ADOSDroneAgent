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
from ados.api.sources.network import latest_uplink_active

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


@router.get("/network")
async def get_ground_station_network() -> dict[str, Any]:
    """Network uplinks view.

    Covers all four uplinks (wifi_client, ethernet, modem_4g) plus the
    active_uplink + priority surfaced by UplinkRouter and the
    share_uplink flag.

    The active_uplink leg reads the durable store first: the native
    ``ados-net`` daemon owns the failover loop and ships its selected uplink as
    a ``net.uplink_active`` event, while the in-FastAPI-process router singleton
    never ticks (its ``active_uplink`` is dead-on-read). On a store gap this
    falls back to the live in-process view, so the leg never 500s. The priority
    list stays the live config-file read, and the ap / wifi_client / ethernet /
    modem_4g / share_uplink legs are unchanged live probes.
    """
    app = _gs._require_ground_profile()
    router_view = _gs._router_state_view()
    store = await latest_uplink_active()
    active_uplink = (
        store.get("active_uplink")
        if store is not None
        else router_view["active_uplink"]
    )
    return {
        "ap": _gs._ap_view(app),
        "wifi_client": await _gs._wifi_client_view(),
        "ethernet": await _gs._ethernet_view(),
        "modem_4g": await _gs._modem_view(),
        "active_uplink": active_uplink,
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


@router.get("/network/modem")
async def get_network_modem() -> dict[str, Any]:
    """Return modem status + data usage + configured cap."""
    _gs._require_ground_profile()
    return await _gs._modem_view()


@router.put("/network/modem")
async def put_network_modem(update: ModemConfigUpdate) -> dict[str, Any]:
    """Update modem config (apn, cap_gb / cap_mb, enabled). Returns refreshed view.

    The manager persists the cap in GB. When the client sends ``cap_mb`` (the
    unit the GET view reports) and no ``cap_gb``, convert it here so a view
    round-trip lands the right cap instead of being silently dropped.
    """
    _gs._require_ground_profile()
    cap_gb = update.cap_gb
    if cap_gb is None and update.cap_mb is not None:
        cap_gb = update.cap_mb / 1024.0
    try:
        await _gs._modem_mgr().configure(
            apn=update.apn,
            cap_gb=cap_gb,
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

    Persists the flag, then applies sysctl + a MASQUERADE rule on the active
    uplink. When the native ``ados-net`` daemon owns the network surface it also
    owns the sysctl + firewall reconciliation, so the in-process apply here would
    be a second writer racing the daemon for the same iptables rule. So, mirroring
    the sibling priority / modem routes, the apply is gated on
    ``is_service_native(net)``: native persists the flag only and lets the daemon
    reconcile; the non-native fallback runs the Python apply.
    """
    _gs._require_ground_profile()
    try:
        _gs._persist_share_uplink_flag(update.enabled)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    from ados.core.runtime_mode import is_service_native

    if is_service_native("net"):
        # The native daemon reconciles the persisted flag itself; do NOT run a
        # second in-process sysctl / iptables apply that would race it.
        return {
            "enabled": bool(update.enabled),
            "applied": True,
            "apply_error": None,
            "backend": "native",
        }

    applied = await _gs._apply_share_uplink(bool(update.enabled))
    result = {
        "enabled": bool(update.enabled),
        "applied": applied["applied"],
        "apply_error": applied.get("apply_error"),
        "backend": applied.get("backend"),
    }
    # When the apply did not land (e.g. no active uplink to MASQUERADE on),
    # surface the short reason so the GCS can tell the operator why.
    if not applied["applied"] and applied.get("reason"):
        result["reason"] = applied["reason"]
    return result
