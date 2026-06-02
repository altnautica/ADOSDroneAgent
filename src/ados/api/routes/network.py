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

import re
import subprocess
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app
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


# ── Stable-MAC pinning ──────────────────────────────────────────────────────
# An onboard adapter with no efuse MAC randomizes its address each boot, churning
# the DHCP lease (and the box's IP). The agent auto-pins a deterministic stable
# MAC for such a chipset (the Rust installer step + supervisor reconciler write a
# next-boot systemd-networkd .link). These routes expose the per-adapter verdicts
# and let an operator confirm a learner candidate, set an explicit override, or
# unpin. The override is stored in network.mac_pin.overrides; the supervisor
# applies it on its next reconcile and fully on the next boot.

_MAC_RE = re.compile(r"^([0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}$")


class MacPinRequest(BaseModel):
    iface: str
    mac: str | None = None
    apply_now: bool = False


def _default_route_iface() -> str | None:
    """The interface carrying the default route (the management path). Used to
    refuse a live re-tag that would drop the operator's own connection."""
    try:
        out = subprocess.run(
            ["ip", "route", "get", "1.1.1.1"],
            capture_output=True,
            text=True,
            timeout=3,
        ).stdout
    except Exception:  # noqa: BLE001
        return None
    parts = out.split()
    if "dev" in parts:
        return parts[parts.index("dev") + 1]
    return None


def _remove_link_file(iface: str) -> bool:
    """Remove this interface's pin .link (just a file; no engine logic needed)."""
    path = Path(f"/etc/systemd/network/10-ados-mac-{iface}.link")
    if not path.exists():
        return False
    try:
        path.unlink()
        subprocess.run(["udevadm", "control", "--reload"], check=False)
        return True
    except OSError:
        return False


@router.get("/mac/adapters")
async def get_mac_adapters() -> dict[str, Any]:
    """Per-adapter stable-MAC verdicts (camelCase, same shape as the heartbeat).

    Returns ``{"version": N, "adapters": [...]}``; an empty list on a board with
    no tracked adapters.
    """
    from ados.services.cloud.heartbeat import (
        _mac_adapter_to_camel,
        read_mac_pins_state,
    )

    raw = read_mac_pins_state() or {}
    adapters = [
        _mac_adapter_to_camel(a)
        for a in (raw.get("adapters") or [])
        if isinstance(a, dict)
    ]
    return {"version": raw.get("version", 1), "adapters": adapters}


@router.post("/mac/pin")
async def post_mac_pin(req: MacPinRequest) -> dict[str, Any]:
    """Pin a stable MAC on an adapter (operator override / candidate confirm).

    Resolves the MAC from ``req.mac`` or the adapter's learner-proposed value,
    stores it as a ``network.mac_pin.overrides`` entry keyed by the interface,
    and persists config; the supervisor applies it on its next reconcile and on
    the next boot. With ``apply_now`` (and ``apply_live_allowed``) it also
    re-tags the LIVE interface -- refused on the management interface so it
    cannot drop the caller's own connection.
    """
    from ados.services.cloud.heartbeat import read_mac_pins_state

    mac = (req.mac or "").strip()
    if not mac:
        for a in (read_mac_pins_state() or {}).get("adapters") or []:
            if isinstance(a, dict) and a.get("name") == req.iface and a.get("pinned_mac"):
                mac = str(a["pinned_mac"])
                break
    if not mac:
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_NO_MAC",
                    "message": "provide a MAC, or pin a candidate that already has a proposed value",
                }
            },
        )
    if not _MAC_RE.match(mac):
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_BAD_MAC", "message": f"malformed MAC: {mac}"}},
        )
    mac = mac.lower().replace("-", ":")

    app = get_agent_app()
    overrides = dict(app.config.network.mac_pin.overrides or {})
    overrides[req.iface] = mac
    app.config.network.mac_pin.overrides = overrides
    try:
        persisted = bool(app.save_config())
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PERSIST", "message": str(exc)}},
        ) from exc

    applied_live = False
    note = "pinned for next boot; the agent writes the .link on its next reconcile"
    if req.apply_now:
        # Resolve the management interface ONCE. _default_route_iface() returns
        # None when detection fails (timeout / parse error); treat that as
        # "uncertain" and REFUSE the live re-tag -- never fall through to it,
        # which could drop the operator's own link if this happens to be the
        # management interface.
        mgmt_iface = _default_route_iface()
        if not app.config.network.mac_pin.apply_live_allowed:
            note = "live re-tag not permitted (set network.mac_pin.apply_live_allowed=true); pinned for next boot"
        elif mgmt_iface is None:
            note = "could not determine the management interface; refusing the live re-tag for safety; pinned for next boot"
        elif req.iface == mgmt_iface:
            note = f"refusing to re-tag {req.iface} live: it carries the management route; pinned for next boot"
        else:
            try:
                for args in (["down"], ["address", mac], ["up"]):
                    subprocess.run(
                        ["ip", "link", "set", "dev", req.iface, *args], check=True
                    )
                applied_live = True
                note = "applied to the live interface now"
            except Exception as exc:  # noqa: BLE001
                note = f"live re-tag failed ({exc}); pinned for next boot"

    return {
        "status": "ok",
        "iface": req.iface,
        "mac": mac,
        "persisted": persisted,
        "appliedLive": applied_live,
        "note": note,
    }


@router.delete("/mac/{iface}")
async def delete_mac_pin(iface: str) -> dict[str, Any]:
    """Unpin an adapter: clear its override and remove the .link.

    Note: a known no-efuse chipset is re-pinned automatically on the next
    reconcile unless ``network.mac_pin.enabled`` is set false.
    """
    app = get_agent_app()
    overrides = dict(app.config.network.mac_pin.overrides or {})
    removed_override = overrides.pop(iface, None) is not None
    if removed_override:
        app.config.network.mac_pin.overrides = overrides
        try:
            app.save_config()
        except Exception:  # noqa: BLE001
            pass
    removed_link = _remove_link_file(iface)
    return {
        "status": "ok",
        "iface": iface,
        "removedOverride": removed_override,
        "removedLinkFile": removed_link,
        "note": "a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false",
    }


__all__ = ["router"]
