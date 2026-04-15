"""Ground-station profile routes (DEC-112, MSN-024/025).

All endpoints gate on `config.agent.profile == "ground_station"` via
`_require_ground_profile()`. Agents running the default drone profile
get 404 with code `E_PROFILE_MISMATCH`.

Phase 0 shipped status snapshot + WFB get/put. Phase 1 (Wave C Cellos)
extends the surface to cover the OLED status schema, network AP
controls, pair key lifecycle, UI config (OLED, buttons, screens), and
factory reset. Spec lives at
`product/specs/ados-ground-agent/11-agent-api-surface.md`.
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path
import re as _re
from typing import Any, Literal

from fastapi import APIRouter, HTTPException, Query, Request, WebSocket, WebSocketDisconnect
from pydantic import BaseModel, Field, field_validator

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# Persistent UI config lives in a separate JSON file because the
# current Pydantic ADOSConfig does not model a ground_station section
# yet. Single file, atomic write, 0644.
_UI_CONFIG_PATH = Path("/etc/ados/ground-station-ui.json")

# Server and OLED both use 0-255 native scale per 2026-04-16 Phase 2
# reconciliation. 204 is roughly 80 percent of 255.
_DEFAULT_OLED: dict[str, Any] = {
    "brightness": 204,
    "auto_dim_enabled": True,
    "screen_cycle_seconds": 5,
}

_DEFAULT_BUTTONS: dict[str, Any] = {
    "mapping": {
        "B1_short": "cycle_screen",
        "B1_long": "toggle_backlight",
        "B2_short": "show_network",
        "B2_long": "show_qr",
        "B3_short": "confirm",
        "B3_long": "pair_drone",
    }
}

_DEFAULT_SCREENS: dict[str, Any] = {
    "order": ["home", "link", "drone", "network", "system", "qr"],
    "enabled": ["home", "link", "drone", "network", "system", "qr"],
}


# ---------------------------------------------------------------------------
# Profile gate + config helpers
# ---------------------------------------------------------------------------


def _require_ground_profile() -> Any:
    """Gate: return the agent app if profile is ground_station, else 404."""
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_PROFILE_MISMATCH"}},
        )
    return app


def _save_config(app: Any) -> None:
    """Best-effort persist agent config to disk."""
    saver = getattr(app, "save_config", None)
    if callable(saver):
        try:
            saver()
            return
        except Exception:
            pass
    cfg_save = getattr(app.config, "save", None)
    if callable(cfg_save):
        try:
            cfg_save()
        except Exception:
            pass


def _load_ui_config() -> dict[str, Any]:
    """Load the UI config blob, filling any missing keys with defaults."""
    data: dict[str, Any] = {}
    try:
        if _UI_CONFIG_PATH.is_file():
            data = json.loads(_UI_CONFIG_PATH.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        data = {}

    oled = {**_DEFAULT_OLED, **(data.get("oled") or {})}
    buttons = {**_DEFAULT_BUTTONS, **(data.get("buttons") or {})}
    screens = {**_DEFAULT_SCREENS, **(data.get("screens") or {})}
    return {"oled": oled, "buttons": buttons, "screens": screens}


def _save_ui_config(data: dict[str, Any]) -> None:
    """Atomic write to the UI config file. Best effort; errors surface as 500."""
    _UI_CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = _UI_CONFIG_PATH.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
    tmp.replace(_UI_CONFIG_PATH)


def _agent_version() -> str:
    try:
        from ados import __version__ as v

        return str(v)
    except Exception:
        return "unknown"


def _system_snapshot() -> dict[str, Any]:
    """CPU, RAM, temp, uptime from psutil with safe fallbacks."""
    out: dict[str, Any] = {
        "cpu_pct": 0.0,
        "ram_used_mb": 0,
        "ram_total_mb": 0,
        "temp_c": None,
        "uptime_seconds": 0,
        "agent_version": _agent_version(),
    }
    try:
        import psutil

        out["cpu_pct"] = float(psutil.cpu_percent(interval=None))
        vm = psutil.virtual_memory()
        out["ram_used_mb"] = int((vm.total - vm.available) / (1024 * 1024))
        out["ram_total_mb"] = int(vm.total / (1024 * 1024))
        out["uptime_seconds"] = int(time.time() - psutil.boot_time())

        temps_fn = getattr(psutil, "sensors_temperatures", None)
        if callable(temps_fn):
            temps = temps_fn() or {}
            preferred = None
            for key in ("cpu_thermal", "coretemp", "soc_thermal", "k10temp"):
                if key in temps and temps[key]:
                    preferred = temps[key][0]
                    break
            if preferred is None:
                for entries in temps.values():
                    if entries:
                        preferred = entries[0]
                        break
            if preferred is not None and preferred.current is not None:
                out["temp_c"] = float(preferred.current)
    except Exception:
        pass
    return out


def _hostapd_manager(app: Any) -> Any:
    """Construct a HostapdManager keyed off the running agent config."""
    from ados.services.ground_station.hostapd_manager import HostapdManager

    device_id = getattr(app.config.agent, "device_id", "unknown")
    hotspot = getattr(app.config.network, "hotspot", None)

    ssid_override: str | None = None
    if hotspot is not None:
        configured = getattr(hotspot, "ssid", "") or ""
        if (
            configured
            and "{device_id}" not in configured
            and configured.startswith("ADOS-GS-")
        ):
            ssid_override = configured

    channel = int(getattr(hotspot, "channel", 6)) if hotspot is not None else 6

    mgr = HostapdManager(
        device_id=device_id,
        ssid=ssid_override,
        channel=channel,
    )
    # Load the persisted passphrase so status() reports a stable SSID + key.
    try:
        mgr.ensure_passphrase()
    except Exception:
        pass
    return mgr


def _pair_manager() -> Any:
    """Return the process-wide PairManager. Lazy import so route module loads without it."""
    from ados.services.ground_station.pair_manager import get_pair_manager

    return get_pair_manager()


def _stock_confirm_token() -> str:
    """Confirmation token used when nothing is currently paired."""
    return "factory-reset-unpaired"


# ---------------------------------------------------------------------------
# Pydantic request models (inline for this route module).
# ---------------------------------------------------------------------------


class WfbUpdate(BaseModel):
    """PUT body for the ground-station WFB radio config."""

    channel: int | None = None
    bitrate_profile: str | None = None
    fec: str | None = None


class ApUpdate(BaseModel):
    """PUT body for the AP subsection of network config."""

    enabled: bool | None = None
    ssid: str | None = None
    passphrase: str | None = None
    channel: int | None = None


class PairRequest(BaseModel):
    """POST body for pair key install."""

    pair_key: str = Field(..., min_length=1)
    drone_device_id: str | None = None


class OledUpdate(BaseModel):
    """PUT body for OLED UI settings.

    Server and OLED both use 0-255 native scale per 2026-04-16 Phase 2
    reconciliation. This matches luma.oled device.contrast() directly
    and the GCS slider range.
    """

    brightness: int | None = Field(default=None, ge=0, le=255)
    auto_dim_enabled: bool | None = None
    screen_cycle_seconds: int | None = Field(default=None, ge=1, le=60)


class ButtonsUpdate(BaseModel):
    """PUT body for button remap. Opaque dict of action bindings."""

    mapping: dict[str, str] | None = None


class ScreensUpdate(BaseModel):
    """PUT body for screen order + enabled list."""

    order: list[str] | None = None
    enabled: list[str] | None = None


# ---------------------------------------------------------------------------
# /status
# ---------------------------------------------------------------------------


def _link_view(app: Any) -> dict[str, Any]:
    """Best-effort link view. Phase 1 fills channel from config; rest stubbed."""
    wfb_cfg = getattr(app.config, "wfb", None)
    channel = getattr(wfb_cfg, "channel", None) if wfb_cfg is not None else None
    return {
        "rssi_dbm": None,
        "bitrate_mbps": None,
        "fec_recovered": 0,
        "fec_lost": 0,
        "channel": channel,
    }


def _network_view(app: Any) -> dict[str, Any]:
    """AP-only view for the OLED status schema."""
    ap_ssid: str | None = None
    ap_ip: str | None = None
    try:
        mgr = _hostapd_manager(app)
        st = mgr.status()
        ap_ssid = st.get("ssid")
        ap_ip = st.get("gateway")
    except Exception:
        pass
    return {
        "ap_ssid": ap_ssid,
        "ap_ip": ap_ip,
        "usb_ip": None,
        "uplink_type": None,
        "uplink_reachable": False,
    }


@router.get("/status")
async def get_ground_station_status() -> dict[str, Any]:
    """Full ground-station snapshot aligned with the OLED schema.

    Wave C Cellos: extended from the Phase 0 stub to match the fields
    the OLED service polls at 1 Hz. Fields that are not yet sourced
    (paired drone telemetry, gcs clients, uplink) return None.
    """
    app = _require_ground_profile()

    # Phase 2: surface the current pair key fingerprint alongside the
    # paired drone id. Source is PairManager.status().
    paired_drone_id: str | None = None
    key_fingerprint: str | None = None
    try:
        pair_status = await _pair_manager().status()
        if pair_status.get("paired"):
            paired_drone_id = pair_status.get("paired_drone_id")
        key_fingerprint = pair_status.get("key_fingerprint")
    except Exception:
        pass

    return {
        "profile": "ground_station",
        "paired_drone": {
            "device_id": paired_drone_id,
            "key_fingerprint": key_fingerprint,
            "fc_mode": None,
            "battery_pct": None,
            "gps_sats": None,
        },
        "link": _link_view(app),
        "gcs": {"clients": [], "pic_id": None},
        "network": _network_view(app),
        "system": _system_snapshot(),
        "recording": False,
    }


# ---------------------------------------------------------------------------
# /wfb
# ---------------------------------------------------------------------------


def _read_wfb_view(app: Any) -> dict[str, Any]:
    wfb_cfg = getattr(app.config, "wfb", None)
    return {
        "channel": getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0,
        "bitrate_profile": getattr(wfb_cfg, "bitrate_profile", "default")
        if wfb_cfg is not None
        else "default",
        "fec": getattr(wfb_cfg, "fec", "8/12") if wfb_cfg is not None else "8/12",
    }


@router.get("/wfb")
async def get_ground_station_wfb() -> dict[str, Any]:
    """Current radio config as stored in agent config."""
    app = _require_ground_profile()
    return _read_wfb_view(app)


@router.put("/wfb")
async def put_ground_station_wfb(update: WfbUpdate) -> dict[str, Any]:
    """Update channel, bitrate profile, or FEC and persist."""
    app = _require_ground_profile()

    wfb_cfg = getattr(app.config, "wfb", None)
    if wfb_cfg is None:
        raise HTTPException(
            status_code=503,
            detail={"error": {"code": "E_WFB_CONFIG_MISSING"}},
        )

    if update.channel is not None and hasattr(wfb_cfg, "channel"):
        setattr(wfb_cfg, "channel", update.channel)
    if update.bitrate_profile is not None and hasattr(wfb_cfg, "bitrate_profile"):
        setattr(wfb_cfg, "bitrate_profile", update.bitrate_profile)
    if update.fec is not None and hasattr(wfb_cfg, "fec"):
        setattr(wfb_cfg, "fec", update.fec)

    _save_config(app)
    return _read_wfb_view(app)


# ---------------------------------------------------------------------------
# /network
# ---------------------------------------------------------------------------


def _ap_view(app: Any) -> dict[str, Any]:
    try:
        mgr = _hostapd_manager(app)
        st = mgr.status()
        return {
            "enabled": bool(st.get("running", False)),
            "running": bool(st.get("running", False)),
            "ssid": st.get("ssid"),
            "channel": st.get("channel"),
            "interface": st.get("interface"),
            "gateway": st.get("gateway"),
            "connected_clients": st.get("connected_clients", []),
        }
    except Exception:
        hotspot = getattr(app.config.network, "hotspot", None)
        return {
            "enabled": False,
            "running": False,
            "ssid": getattr(hotspot, "ssid", None) if hotspot is not None else None,
            "channel": getattr(hotspot, "channel", None)
            if hotspot is not None
            else None,
            "interface": None,
            "gateway": None,
            "connected_clients": [],
        }


async def _wifi_client_view() -> dict[str, Any]:
    """Wave C Cellos: surface WifiClientManager status + enabled_on_boot."""
    try:
        from ados.services.ground_station.wifi_client_manager import (
            get_wifi_client_manager,
        )

        mgr = get_wifi_client_manager()
        st = await mgr.status()
        cfg = mgr._load_client_config()  # internal helper, ok for a view
        return {
            "enabled_on_boot": bool(cfg.get("enabled_on_boot", False)),
            "connected": bool(st.get("connected", False)),
            "ssid": st.get("ssid"),
            "signal": st.get("signal"),
            "ip": st.get("ip"),
        }
    except Exception:
        return {
            "enabled_on_boot": False,
            "connected": False,
            "ssid": None,
            "signal": None,
            "ip": None,
        }


async def _ethernet_view() -> dict[str, Any]:
    try:
        from ados.services.ground_station.ethernet_manager import (
            get_ethernet_manager,
        )

        st = await get_ethernet_manager().status()
        return {
            "link": bool(st.get("link", False)),
            "speed_mbps": st.get("speed_mbps"),
            "ip": st.get("ip"),
            "gateway": st.get("gateway"),
        }
    except Exception:
        return {"link": False, "speed_mbps": None, "ip": None, "gateway": None}


async def _modem_view() -> dict[str, Any]:
    try:
        from ados.services.ground_station.modem_manager import (
            get_modem_manager,
        )

        mgr = get_modem_manager()
        st = await mgr.status()
        usage = await mgr.data_usage()
        cfg = getattr(mgr, "_config", {}) or {}

        cap_gb = cfg.get("cap_gb")
        try:
            cap_mb = int(float(cap_gb) * 1024) if cap_gb is not None else 0
        except (TypeError, ValueError):
            cap_mb = 0
        total_bytes = int(usage.get("total_bytes", 0) or 0)
        data_used_mb = int(total_bytes / (1024 * 1024)) if total_bytes else 0
        percent = (data_used_mb / cap_mb * 100.0) if cap_mb else 0.0

        return {
            "enabled": bool(cfg.get("enabled", False)),
            "connected": bool(st.get("connected", False)),
            "iface": st.get("iface"),
            "ip": st.get("ip"),
            "signal_quality": st.get("signal_quality"),
            "technology": st.get("technology"),
            "apn": st.get("apn") or cfg.get("apn"),
            "operator": st.get("operator"),
            "data_used_mb": data_used_mb,
            "cap_mb": cap_mb,
            "percent": round(percent, 2),
            "state": "connected" if st.get("connected") else "disconnected",
        }
    except Exception:
        return {
            "enabled": False,
            "connected": False,
            "iface": None,
            "ip": None,
            "signal_quality": None,
            "technology": None,
            "apn": None,
            "operator": None,
            "data_used_mb": 0,
            "cap_mb": 0,
            "percent": 0.0,
            "state": "unknown",
        }


def _router_state_view() -> dict[str, Any]:
    """Active uplink + priority list from UplinkRouter singleton."""
    try:
        from ados.services.ground_station.uplink_router import get_uplink_router

        router = get_uplink_router()
        return {
            "active_uplink": router.active_uplink,
            "priority": list(router.get_priority()),
        }
    except Exception:
        return {"active_uplink": None, "priority": []}


def _load_share_uplink_flag() -> bool:
    """Read share_uplink from the Pydantic-backed ADOSConfig.

    Phase 4 Wave 1: authoritative source is now
    `ADOSConfig.ground_station.share_uplink` (YAML). The legacy JSON
    side-file at `_UI_CONFIG_PATH` is handled by the one-shot migrator
    in `ados.core.config.load_config()` and preserved on disk.
    """
    try:
        from ados.core.config import load_config

        cfg = load_config()
        return bool(cfg.ground_station.share_uplink)
    except Exception:
        return False


@router.get("/network")
async def get_ground_station_network() -> dict[str, Any]:
    """Network uplinks view.

    Wave C Cellos expands this from the Phase 1 AP-only stub to cover
    all four uplinks (wifi_client, ethernet, modem_4g) plus the
    active_uplink + priority surfaced by UplinkRouter and the
    share_uplink flag.
    """
    app = _require_ground_profile()
    router_view = _router_state_view()
    return {
        "ap": _ap_view(app),
        "wifi_client": await _wifi_client_view(),
        "ethernet": await _ethernet_view(),
        "modem_4g": await _modem_view(),
        "active_uplink": router_view["active_uplink"],
        "priority": router_view["priority"],
        "share_uplink": _load_share_uplink_flag(),
    }


@router.put("/network/ap")
async def put_ground_station_ap(update: ApUpdate) -> dict[str, Any]:
    """Apply AP config change via HostapdManager.apply_ap_config()."""
    app = _require_ground_profile()

    mgr = _hostapd_manager(app)
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
        _save_config(app)

    return _ap_view(app)


# ---------------------------------------------------------------------------
# /network/ethernet
# ---------------------------------------------------------------------------


_IPV4_RE = _re.compile(
    r"^((25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$"
)


def _validate_ipv4(value: str) -> bool:
    return bool(_IPV4_RE.match(value))


def _validate_ipv4_cidr(value: str) -> bool:
    if "/" not in value:
        return False
    addr, _, prefix = value.partition("/")
    if not _validate_ipv4(addr):
        return False
    try:
        p = int(prefix)
    except ValueError:
        return False
    return 0 <= p <= 32


class EthernetConfigUpdate(BaseModel):
    """PUT body for /network/ethernet."""

    mode: Literal["dhcp", "static"]
    ip: str | None = None
    gateway: str | None = None
    dns: list[str] | None = None

    @field_validator("ip")
    @classmethod
    def _v_ip(cls, v: str | None) -> str | None:
        if v is None or v == "":
            return None
        if not _validate_ipv4_cidr(v):
            raise ValueError("ip must be IPv4 with CIDR suffix, e.g. 192.168.1.42/24")
        return v

    @field_validator("gateway")
    @classmethod
    def _v_gateway(cls, v: str | None) -> str | None:
        if v is None or v == "":
            return None
        if not _validate_ipv4(v):
            raise ValueError("gateway must be a valid IPv4 address")
        return v

    @field_validator("dns")
    @classmethod
    def _v_dns(cls, v: list[str] | None) -> list[str] | None:
        if v is None:
            return None
        for entry in v:
            if not _validate_ipv4(entry):
                raise ValueError(f"dns entry {entry!r} is not a valid IPv4 address")
        return v


def _ethernet_mgr() -> Any:
    from ados.services.ground_station.ethernet_manager import (
        get_ethernet_manager,
    )

    return get_ethernet_manager()


@router.get("/network/ethernet")
async def get_network_ethernet() -> dict[str, Any]:
    """Return the configured Ethernet profile plus live link state."""
    _require_ground_profile()
    try:
        return await _ethernet_mgr().config()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_ETHERNET_CONFIG_READ_FAILED", "message": str(exc)}},
        ) from exc


@router.put("/network/ethernet")
async def put_network_ethernet(update: EthernetConfigUpdate) -> dict[str, Any]:
    """Apply Ethernet IPv4 config. mode=dhcp or mode=static."""
    _require_ground_profile()
    mgr = _ethernet_mgr()

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


# ---------------------------------------------------------------------------
# /wfb/pair
# ---------------------------------------------------------------------------


@router.post("/wfb/pair")
async def post_wfb_pair(req: PairRequest) -> dict[str, Any]:
    """Install a drone pair key. 409 if already paired."""
    _require_ground_profile()

    pm = _pair_manager()

    try:
        current = await pm.status()
    except Exception:
        current = {"paired": False}

    if current.get("paired"):
        raise HTTPException(
            status_code=409,
            detail={
                "error": {
                    "code": "E_ALREADY_PAIRED",
                    "message": "unpair before pairing a new drone",
                    "paired_drone_id": current.get("paired_drone_id"),
                }
            },
        )

    try:
        result = await pm.pair(
            pair_key=req.pair_key,
            drone_device_id=req.drone_device_id,
        )
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_INVALID_PAIR_KEY", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_FAILED", "message": str(exc)}},
        ) from exc

    return result


@router.delete("/wfb/pair")
async def delete_wfb_pair() -> dict[str, Any]:
    """Remove the installed pair key."""
    _require_ground_profile()

    pm = _pair_manager()
    try:
        return await pm.unpair()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UNPAIR_FAILED", "message": str(exc)}},
        ) from exc


# ---------------------------------------------------------------------------
# /ui
# ---------------------------------------------------------------------------


@router.get("/ui")
async def get_ground_station_ui() -> dict[str, Any]:
    """Return the full UI config (OLED, buttons, screens)."""
    _require_ground_profile()
    return _load_ui_config()


def _persist_gs_ui_section(section: str, value: dict[str, Any]) -> None:
    """Write `ground_station.ui.<section>` into the YAML-backed ADOSConfig.

    Phase 4 Wave 2: the OLED, button, and screen UI config now lives in
    the Pydantic model so it round-trips through save cycles and is
    consumed by the live services. The legacy JSON side-file is no
    longer written, but remains on disk for rollback (the load-time
    migrator preserves it).
    """
    from ados.services.ground_station.pair_manager import (
        _load_config_dict,
        _save_config_dict,
    )

    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section
    ui_section = gs_section.get("ui")
    if not isinstance(ui_section, dict):
        ui_section = {}
        gs_section["ui"] = ui_section
    ui_section[section] = value
    if not _save_config_dict(data):
        raise OSError("failed to persist ground_station.ui to /etc/ados/config.yaml")


def _refresh_in_memory_ui(app: Any, section: str, value: dict[str, Any]) -> None:
    """Mirror the persisted section into the running app config."""
    try:
        gs = getattr(app.config, "ground_station", None)
        if gs is None:
            return
        ui = getattr(gs, "ui", None)
        if ui is None:
            return
        if hasattr(ui, section):
            setattr(ui, section, dict(value))
    except Exception:
        pass


@router.put("/ui/oled")
async def put_ground_station_ui_oled(update: OledUpdate) -> dict[str, Any]:
    """Update OLED settings, persist to config.yaml, signal oled_service."""
    app = _require_ground_profile()

    data = _load_ui_config()
    oled = dict(data["oled"])
    if update.brightness is not None:
        oled["brightness"] = update.brightness
    if update.auto_dim_enabled is not None:
        oled["auto_dim_enabled"] = update.auto_dim_enabled
    if update.screen_cycle_seconds is not None:
        oled["screen_cycle_seconds"] = update.screen_cycle_seconds
    data["oled"] = oled

    try:
        _persist_gs_ui_section("oled", oled)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _refresh_in_memory_ui(app, "oled", oled)

    from ados.services.ui.reload_signal import signal_oled_reload

    signal_oled_reload()
    return data


@router.put("/ui/buttons")
async def put_ground_station_ui_buttons(update: ButtonsUpdate) -> dict[str, Any]:
    """Replace the button mapping. Persisted to config and SIGHUP'd live."""
    app = _require_ground_profile()

    data = _load_ui_config()
    if update.mapping is not None:
        data["buttons"] = {"mapping": dict(update.mapping)}

    try:
        _persist_gs_ui_section("buttons", data["buttons"])
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _refresh_in_memory_ui(app, "buttons", data["buttons"])

    from ados.services.ui.reload_signal import signal_buttons_reload

    signal_buttons_reload()
    return data


@router.put("/ui/screens")
async def put_ground_station_ui_screens(update: ScreensUpdate) -> dict[str, Any]:
    """Update screen order and/or enabled set. SIGHUPs oled_service live."""
    app = _require_ground_profile()

    data = _load_ui_config()
    screens = dict(data["screens"])
    if update.order is not None:
        screens["order"] = list(update.order)
    if update.enabled is not None:
        screens["enabled"] = list(update.enabled)
    data["screens"] = screens

    try:
        _persist_gs_ui_section("screens", screens)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    _refresh_in_memory_ui(app, "screens", screens)

    from ados.services.ui.reload_signal import signal_oled_reload

    signal_oled_reload()
    return data


# ---------------------------------------------------------------------------
# /captive-token
# ---------------------------------------------------------------------------


def _is_ap_subnet_client(host: str | None) -> bool:
    """True when the request came from the AP subnet 192.168.4.0/24.

    POC check: string-prefix match on the hotspot subnet. Loopback is
    also allowed so the agent itself and local tooling can mint a
    token for tests. Anything else is rejected with 403.
    """
    if not host:
        return False
    if host == "127.0.0.1" or host == "::1":
        return True
    return host.startswith("192.168.4.")


@router.get("/captive-token")
async def get_captive_token(request: Request) -> dict[str, Any]:
    """Mint a single-use captive-portal token for the setup webapp.

    Gated on the AP subnet (192.168.4.0/24). Hosts connecting over any
    other interface get 403. The token is attached by the webapp as
    `X-ADOS-Captive-Key` on destructive operations.
    """
    _require_ground_profile()

    client_host = request.client.host if request.client else None
    if not _is_ap_subnet_client(client_host):
        raise HTTPException(
            status_code=403,
            detail={"error": {"code": "E_CAPTIVE_ONLY"}},
        )

    from ados.services.setup_webapp.captive_token import get_captive_token_store

    token = get_captive_token_store().generate()
    return {"token": token}


# ---------------------------------------------------------------------------
# /factory-reset
# ---------------------------------------------------------------------------


@router.post("/factory-reset")
async def post_factory_reset(
    request: Request,
    confirm: str = Query(..., description="Current pair key fingerprint or stock token"),
) -> dict[str, Any]:
    """Wipe pair state and AP passphrase. Requires the current fingerprint.

    When the ground station is paired, the confirm token must match the
    active pair key fingerprint. When unpaired, the token must match
    `factory-reset-unpaired`. This stops a casual curl from bricking a
    live device.
    """
    _require_ground_profile()

    # Captive-portal single-use token check. Phase 2: only factory
    # reset is gated. The header is optional when called from loopback
    # to keep CLI test paths open.
    captive_header = request.headers.get("x-ados-captive-key")
    client_host = request.client.host if request.client else None
    if client_host not in ("127.0.0.1", "::1"):
        from ados.services.setup_webapp.captive_token import get_captive_token_store

        if not captive_header or not get_captive_token_store().consume(captive_header):
            raise HTTPException(
                status_code=401,
                detail={"error": {"code": "E_CAPTIVE_TOKEN_INVALID"}},
            )

    pm = _pair_manager()

    try:
        current = await pm.status()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_STATUS_FAILED", "message": str(exc)}},
        ) from exc

    expected = current.get("key_fingerprint") or _stock_confirm_token()
    if confirm != expected:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_CONFIRM_MISMATCH"}},
        )

    try:
        return await pm.factory_reset()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_FACTORY_RESET_FAILED", "message": str(exc)}},
        ) from exc


# ---------------------------------------------------------------------------
# Phase 2 (MSN-026 Wave C Cellos): display, input, PIC arbiter.
# ---------------------------------------------------------------------------


_DEFAULT_DISPLAY: dict[str, Any] = {
    "resolution": "auto",
    "kiosk_enabled": False,
    "kiosk_target_url": None,
}


def _load_display_config() -> dict[str, Any]:
    """Read display section of the persistent UI config blob."""
    data: dict[str, Any] = {}
    try:
        if _UI_CONFIG_PATH.is_file():
            data = json.loads(_UI_CONFIG_PATH.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        data = {}
    display = {**_DEFAULT_DISPLAY, **(data.get("display") or {})}
    return display


def _save_display_config(display: dict[str, Any]) -> None:
    """Merge the new display blob back into the UI config file."""
    data: dict[str, Any] = {}
    try:
        if _UI_CONFIG_PATH.is_file():
            data = json.loads(_UI_CONFIG_PATH.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        data = {}
    data["display"] = display
    _UI_CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = _UI_CONFIG_PATH.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
    tmp.replace(_UI_CONFIG_PATH)


class DisplayUpdate(BaseModel):
    """PUT body for HDMI kiosk display config."""

    resolution: str | None = Field(default=None)
    kiosk_enabled: bool | None = None
    kiosk_target_url: str | None = None


class BluetoothScanRequest(BaseModel):
    """POST body for the Bluetooth scan endpoint."""

    duration_s: int | None = Field(default=None, ge=1, le=60)


class BluetoothPairRequest(BaseModel):
    """POST body for Bluetooth pairing."""

    mac: str = Field(..., min_length=1)


class GamepadPrimaryUpdate(BaseModel):
    """PUT body for primary-gamepad selection."""

    device_id: str = Field(..., min_length=1)


class PicClaimRequest(BaseModel):
    """POST body for PIC claim."""

    client_id: str = Field(..., min_length=1)
    confirm_token: str | None = None
    force: bool | None = False


class PicReleaseRequest(BaseModel):
    """POST body for PIC release."""

    client_id: str = Field(..., min_length=1)


class PicConfirmTokenRequest(BaseModel):
    """POST body for PIC confirm-token mint."""

    client_id: str = Field(..., min_length=1)


class PicHeartbeatRequest(BaseModel):
    """POST body for PIC session heartbeat."""

    client_id: str = Field(..., min_length=1)


# ── /display ───────────────────────────────────────────────────────────────


@router.get("/display")
async def get_ground_station_display() -> dict[str, Any]:
    """Return the persisted HDMI kiosk display config."""
    _require_ground_profile()
    return _load_display_config()


@router.put("/display")
async def put_ground_station_display(update: DisplayUpdate) -> dict[str, Any]:
    """Update the HDMI kiosk display config and persist."""
    _require_ground_profile()
    current = _load_display_config()

    allowed_res = {"auto", "720p", "1080p"}
    if update.resolution is not None:
        if update.resolution not in allowed_res:
            raise HTTPException(
                status_code=400,
                detail={"error": {"code": "E_INVALID_RESOLUTION"}},
            )
        current["resolution"] = update.resolution
    if update.kiosk_enabled is not None:
        current["kiosk_enabled"] = bool(update.kiosk_enabled)
    if update.kiosk_target_url is not None:
        current["kiosk_target_url"] = update.kiosk_target_url

    try:
        _save_display_config(current)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc
    return current


# ── /bluetooth ─────────────────────────────────────────────────────────────


def _input_manager() -> Any:
    """Lazy import helper for the InputManager singleton."""
    from ados.services.ground_station.input_manager import get_input_manager

    return get_input_manager()


@router.post("/bluetooth/scan")
async def post_bluetooth_scan(req: BluetoothScanRequest) -> dict[str, Any]:
    """Run a BlueZ scan for nearby gamepads. Default duration 10 s."""
    _require_ground_profile()

    duration = req.duration_s if req.duration_s is not None else 10
    try:
        devices = await _input_manager().scan_bluetooth(duration_s=duration)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_SCAN_FAILED", "message": str(exc)}},
        ) from exc
    return {"devices": devices or []}


@router.post("/bluetooth/pair")
async def post_bluetooth_pair(req: BluetoothPairRequest) -> dict[str, Any]:
    """Attempt to pair with a Bluetooth device by MAC address."""
    _require_ground_profile()

    try:
        result = await _input_manager().pair_bluetooth(req.mac)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_PAIR_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"paired": bool(result), "error": None}


@router.delete("/bluetooth/{mac}")
async def delete_bluetooth(mac: str) -> dict[str, Any]:
    """Forget a previously-paired Bluetooth device."""
    _require_ground_profile()

    try:
        result = await _input_manager().forget_bluetooth(mac)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_FORGET_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"forgotten": bool(result)}


@router.get("/bluetooth/paired")
async def get_bluetooth_paired() -> dict[str, Any]:
    """List paired Bluetooth devices."""
    _require_ground_profile()

    try:
        devices = await _input_manager().paired_bluetooth()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_BT_LIST_FAILED", "message": str(exc)}},
        ) from exc
    return {"devices": devices or []}


# ── /gamepads ──────────────────────────────────────────────────────────────


@router.get("/gamepads")
async def get_gamepads() -> dict[str, Any]:
    """List connected gamepads and the current primary device id."""
    _require_ground_profile()

    mgr = _input_manager()
    try:
        devices = mgr.list_gamepads()
        if asyncio.iscoroutine(devices):
            devices = await devices
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_GAMEPAD_LIST_FAILED", "message": str(exc)}},
        ) from exc

    primary_id: str | None = None
    try:
        primary = mgr.get_primary()
        if asyncio.iscoroutine(primary):
            primary = await primary
        if isinstance(primary, dict):
            primary_id = primary.get("device_id") or primary.get("id")
        elif isinstance(primary, str):
            primary_id = primary
    except Exception:
        primary_id = None

    return {"devices": devices or [], "primary_id": primary_id}


@router.put("/gamepads/primary")
async def put_gamepad_primary(update: GamepadPrimaryUpdate) -> dict[str, Any]:
    """Select the primary gamepad used by the PIC arbiter."""
    _require_ground_profile()

    try:
        result = _input_manager().set_primary(update.device_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_GAMEPAD_PRIMARY_FAILED", "message": str(exc)}},
        ) from exc

    return {"primary_id": update.device_id, "result": result}


# ── /pic ───────────────────────────────────────────────────────────────────


def _pic_arbiter() -> Any:
    """Lazy import helper for the PicArbiter singleton."""
    from ados.services.ground_station.pic_arbiter import get_pic_arbiter

    return get_pic_arbiter()


@router.get("/pic")
async def get_pic_state() -> dict[str, Any]:
    """Return the current PIC state dict."""
    _require_ground_profile()

    try:
        state = _pic_arbiter().get_state()
        if asyncio.iscoroutine(state):
            state = await state
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_STATE_FAILED", "message": str(exc)}},
        ) from exc
    return state if isinstance(state, dict) else {"state": state}


@router.post("/pic/claim")
async def post_pic_claim(req: PicClaimRequest) -> dict[str, Any]:
    """Claim PIC. Returns 409 with needs_confirm=True when re-claim is required."""
    _require_ground_profile()

    arb = _pic_arbiter()
    try:
        result = arb.claim(
            req.client_id,
            confirm_token=req.confirm_token,
            force=bool(req.force),
        )
        if asyncio.iscoroutine(result):
            result = await result
    except PermissionError as exc:
        # Raised when another client holds PIC and no confirm token was
        # provided. Signal the caller to mint a confirm token and retry.
        raise HTTPException(
            status_code=409,
            detail={
                "error": {"code": "E_PIC_CONFIRM_REQUIRED", "message": str(exc)},
                "needs_confirm": True,
            },
        ) from exc
    except ValueError as exc:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_PIC_CLAIM_INVALID", "message": str(exc)}},
        ) from exc
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_CLAIM_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        if result.get("needs_confirm") and not result.get("granted"):
            # Soft-reject path: arbiter returns dict rather than raising.
            return {**result, "needs_confirm": True}
        return result
    return {"granted": bool(result), "client_id": req.client_id}


@router.post("/pic/release")
async def post_pic_release(req: PicReleaseRequest) -> dict[str, Any]:
    """Release PIC held by the given client id."""
    _require_ground_profile()

    try:
        result = _pic_arbiter().release(req.client_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_RELEASE_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict):
        return result
    return {"released": bool(result), "client_id": req.client_id}


@router.post("/pic/confirm-token")
async def post_pic_confirm_token(req: PicConfirmTokenRequest) -> dict[str, Any]:
    """Mint a short-lived PIC takeover confirmation token."""
    _require_ground_profile()

    try:
        token = _pic_arbiter().create_confirm_token(req.client_id)
        if asyncio.iscoroutine(token):
            token = await token
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_TOKEN_FAILED", "message": str(exc)}},
        ) from exc

    value: str
    ttl: int = 2
    if isinstance(token, dict):
        value = str(token.get("token", ""))
        ttl = int(token.get("ttl_seconds", 2))
    else:
        value = str(token)

    return {"token": value, "ttl_seconds": ttl}


@router.post("/pic/heartbeat")
async def post_pic_heartbeat(req: PicHeartbeatRequest) -> dict[str, Any]:
    """Refresh the PIC session TTL. 410 if the client does not hold PIC."""
    _require_ground_profile()

    try:
        result = _pic_arbiter().heartbeat(req.client_id)
        if asyncio.iscoroutine(result):
            result = await result
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PIC_HEARTBEAT_FAILED", "message": str(exc)}},
        ) from exc

    if isinstance(result, dict) and result.get("ok") is False:
        raise HTTPException(
            status_code=int(result.get("status", 410)),
            detail={
                "error": {
                    "code": "E_PIC_NO_ACTIVE_CLAIM",
                    "message": str(result.get("error", "no active claim")),
                    "current_pic": result.get("current_pic"),
                }
            },
        )
    return result if isinstance(result, dict) else {"ok": True}


@router.websocket("/pic/events")
async def ws_pic_events(websocket: WebSocket) -> None:
    """Stream PIC arbiter events as JSON until the client disconnects."""
    # Profile gate before accepting so wrong-profile agents close 1008.
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    await websocket.accept()

    # Lazy import to avoid a circular at module load.
    from ados.services.ground_station.pic_arbiter import get_pic_arbiter as _gpa

    arb = _gpa()
    bus = getattr(arb, "bus", None) or getattr(arb, "event_bus", None)
    if bus is None:
        await websocket.send_json({
            "event": "error",
            "code": "E_PIC_BUS_UNAVAILABLE",
        })
        await websocket.close()
        return

    queue: asyncio.Queue[Any] = asyncio.Queue()

    def _on_event(payload: Any) -> None:
        try:
            queue.put_nowait(payload)
        except asyncio.QueueFull:
            pass

    unsubscribe: Any = None
    try:
        subscribe = getattr(bus, "subscribe", None)
        if callable(subscribe):
            unsubscribe = subscribe(_on_event)
    except Exception:
        unsubscribe = None

    try:
        while True:
            payload = await queue.get()
            try:
                await websocket.send_json(payload if isinstance(payload, dict) else {"event": payload})
            except (WebSocketDisconnect, RuntimeError):
                break
    except WebSocketDisconnect:
        pass
    finally:
        if callable(unsubscribe):
            try:
                unsubscribe()
            except Exception:  # noqa: BLE001
                pass


# ---------------------------------------------------------------------------
# Phase 3 (MSN-027 Wave C Cellos): network uplinks (wifi client, modem,
# uplink router priority + share_uplink toggle) + uplink event stream.
# ---------------------------------------------------------------------------


_UPLINK_PRIORITY_PATH = Path("/etc/ados/ground-station-uplink.json")


class WifiJoinRequest(BaseModel):
    """PUT body for /network/client/join."""

    ssid: str = Field(..., min_length=1)
    passphrase: str | None = None
    force: bool | None = False


class ModemConfigUpdate(BaseModel):
    """PUT body for /network/modem."""

    apn: str | None = None
    cap_gb: float | None = Field(default=None, gt=0, le=9223372036.0)
    enabled: bool | None = None


class UplinkPriorityUpdate(BaseModel):
    """PUT body for /network/priority."""

    priority: list[str] = Field(..., min_length=1)


class ShareUplinkUpdate(BaseModel):
    """PUT body for /network/share_uplink."""

    enabled: bool


def _wifi_client_manager() -> Any:
    from ados.services.ground_station.wifi_client_manager import (
        get_wifi_client_manager,
    )

    return get_wifi_client_manager()


def _modem_mgr() -> Any:
    from ados.services.ground_station.modem_manager import get_modem_manager

    return get_modem_manager()


def _uplink_router() -> Any:
    from ados.services.ground_station.uplink_router import get_uplink_router

    return get_uplink_router()


# ── /network/client ────────────────────────────────────────────────────────


@router.get("/network/client/scan")
async def get_network_client_scan() -> dict[str, Any]:
    """Scan for nearby WiFi networks via nmcli."""
    _require_ground_profile()
    try:
        networks = await _wifi_client_manager().scan(timeout_s=10)
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_SCAN_FAILED", "message": str(exc)}},
        ) from exc
    return {"networks": networks or []}


@router.put("/network/client/join")
async def put_network_client_join(req: WifiJoinRequest) -> dict[str, Any]:
    """Join a WiFi network. 409 on AP mutex conflict without force."""
    _require_ground_profile()

    try:
        result = await _wifi_client_manager().join(
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
    _require_ground_profile()
    try:
        return await _wifi_client_manager().leave()
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_WIFI_LEAVE_FAILED", "message": str(exc)}},
        ) from exc


# ── /network/modem ─────────────────────────────────────────────────────────


@router.get("/network/modem")
async def get_network_modem() -> dict[str, Any]:
    """Return modem status + data usage + configured cap."""
    _require_ground_profile()
    return await _modem_view()


@router.put("/network/modem")
async def put_network_modem(update: ModemConfigUpdate) -> dict[str, Any]:
    """Update modem config (apn, cap_gb, enabled). Returns refreshed view."""
    _require_ground_profile()
    try:
        await _modem_mgr().configure(
            apn=update.apn,
            cap_gb=update.cap_gb,
            enabled=update.enabled,
        )
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_MODEM_CONFIGURE_FAILED", "message": str(exc)}},
        ) from exc
    return await _modem_view()


# ── /network/priority ──────────────────────────────────────────────────────


@router.get("/network/priority")
async def get_network_priority() -> dict[str, Any]:
    """Return the current uplink priority list."""
    _require_ground_profile()
    try:
        priority = list(_uplink_router().get_priority())
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UPLINK_PRIORITY_FAILED", "message": str(exc)}},
        ) from exc
    return {"priority": priority}


@router.put("/network/priority")
async def put_network_priority(update: UplinkPriorityUpdate) -> dict[str, Any]:
    """Set the uplink priority list. Router persists to its own JSON."""
    _require_ground_profile()
    try:
        _uplink_router().set_priority(list(update.priority))
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
    return {"priority": list(_uplink_router().get_priority())}


# ── /network/share_uplink ──────────────────────────────────────────────────


def _persist_share_uplink_flag(enabled: bool) -> None:
    """Write share_uplink into the Pydantic-backed ADOSConfig on disk.

    Phase 4 Wave 1: writes to `/etc/ados/config.yaml` under
    `ground_station.share_uplink`. The legacy JSON side-file is NOT
    written, but is preserved on disk for rollback. The pair_manager
    atomic save helper is reused so air and ground paths share one
    code path.
    """
    from ados.services.ground_station.pair_manager import (
        _load_config_dict,
        _save_config_dict,
    )

    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section
    gs_section["share_uplink"] = bool(enabled)
    if not _save_config_dict(data):
        raise OSError("failed to persist share_uplink to /etc/ados/config.yaml")


async def _apply_share_uplink(enabled: bool) -> dict[str, Any]:
    """Apply sysctl + NAT and persist firewall state across reboots.

    Phase 4 Wave 2 Cellos: delegates to
    `services/ground_station/share_uplink_firewall.apply_share_uplink`
    which handles distro detection, iptables-persistent vs nftables
    fallback, atomic sysctl drop-in, and persistence of the rule set.
    Phase 3 inline POC is replaced; signature preserved for callers.
    """
    active_iface: str | None = None
    try:
        router_ = _uplink_router()
        active_name = router_.active_uplink
        if active_name:
            mgr = await router_._manager_for(active_name)  # type: ignore[attr-defined]
            if mgr is not None:
                get_iface = getattr(mgr, "get_iface", None)
                if callable(get_iface):
                    active_iface = get_iface()
    except Exception:
        active_iface = None

    try:
        from ados.services.ground_station.share_uplink_firewall import (
            apply_share_uplink as _apply,
        )
        result = await _apply(bool(enabled), active_iface)
    except Exception as exc:
        return {"applied": False, "apply_error": f"firewall_helper_failed: {exc}"}

    return {
        "applied": bool(result.get("applied", False)),
        "apply_error": result.get("apply_error"),
        "backend": result.get("backend"),
    }


@router.put("/network/share_uplink")
async def put_network_share_uplink(update: ShareUplinkUpdate) -> dict[str, Any]:
    """Toggle IPv4 forwarding + NAT masquerade for AP clients.

    POC implementation: writes net.ipv4.ip_forward via sysctl and adds
    a MASQUERADE rule on the active uplink. On failure the flag is
    still persisted and the error is surfaced in the response. Full
    firewall management comes in a later phase.
    """
    _require_ground_profile()
    try:
        _persist_share_uplink_flag(update.enabled)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc

    applied = await _apply_share_uplink(bool(update.enabled))
    return {
        "enabled": bool(update.enabled),
        "applied": applied["applied"],
        "apply_error": applied["apply_error"],
        "backend": applied.get("backend"),
    }


# ── /ws/uplink ─────────────────────────────────────────────────────────────


@router.websocket("/ws/uplink")
async def ws_uplink_events(websocket: WebSocket) -> None:
    """Stream UplinkRouter events as JSON until the client disconnects.

    Mirrors the `/pic/events` pattern: profile-gate before accept so
    wrong-profile callers close with 1008; subscribe to the async
    iterator `UplinkEventBus.subscribe()`; JSON-serialize each event.
    """
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    await websocket.accept()

    try:
        from ados.services.ground_station.uplink_router import get_uplink_router
    except Exception:
        await websocket.send_json({"event": "error", "code": "E_UPLINK_ROUTER_UNAVAILABLE"})
        await websocket.close()
        return

    try:
        bus = get_uplink_router().bus
    except Exception:
        await websocket.send_json({"event": "error", "code": "E_UPLINK_BUS_UNAVAILABLE"})
        await websocket.close()
        return

    try:
        async for evt in bus.subscribe():
            try:
                payload = {
                    "kind": evt.kind,
                    "active_uplink": evt.active_uplink,
                    "available": list(evt.available) if evt.available is not None else [],
                    "internet_reachable": bool(evt.internet_reachable),
                    "data_cap_state": evt.data_cap_state,
                    "timestamp_ms": evt.timestamp_ms,
                }
                await websocket.send_json(payload)
            except (WebSocketDisconnect, RuntimeError):
                break
    except WebSocketDisconnect:
        pass
    except Exception:
        # Bus closed or subscriber removed under us.
        pass
