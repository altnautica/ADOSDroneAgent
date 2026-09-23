"""Profile-agnostic Wi-Fi client REST surface (the residual part).

Lives outside the ground-station namespace because Wi-Fi client
operations only depend on a wlan interface being present, not on the
operator's chosen profile.

The station status and the join / leave / forget / autoconnect writes are
served by the native front on every profile, through the Wi-Fi command
socket. Only the scan and the stable-MAC routes remain here.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app

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
    write = app.save_config()
    if not write:
        # A pin that never reached disk is gone at the next reconcile. Fail the
        # call rather than answering `persisted: false` alongside a 200 that
        # every caller reads as success.
        raise HTTPException(
            status_code=500,
            detail={
                "error": {
                    "code": "E_PERSIST",
                    "message": write.error or "the agent could not persist this pin",
                }
            },
        )
    persisted = True

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
                    subprocess.run(["ip", "link", "set", "dev", req.iface, *args], check=True)
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
    persist_error: str | None = None
    if removed_override:
        app.config.network.mac_pin.overrides = overrides
        # `overrides` is a free-form mapping, so the writer replaces the whole
        # value rather than merging it — which is what makes the removal
        # actually leave the file.
        write = app.save_config()
        if not write:
            removed_override = False
            persist_error = write.error or "the agent could not persist this change"
    removed_link = _remove_link_file(iface)
    response: dict[str, Any] = {
        "status": "ok",
        "iface": iface,
        "removedOverride": removed_override,
        "removedLinkFile": removed_link,
        "note": "a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false",
    }
    if persist_error is not None:
        response["persistError"] = persist_error
    return response


__all__ = ["router"]
