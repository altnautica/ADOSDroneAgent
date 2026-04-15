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

import json
import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# Persistent UI config lives in a separate JSON file because the
# current Pydantic ADOSConfig does not model a ground_station section
# yet. Single file, atomic write, 0644.
_UI_CONFIG_PATH = Path("/etc/ados/ground-station-ui.json")

_DEFAULT_OLED: dict[str, Any] = {
    "brightness": 80,
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
    """Construct a PairManager. Lazy import so route module loads without it."""
    from ados.services.ground_station.pair_manager import PairManager

    return PairManager()


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
    """PUT body for OLED UI settings."""

    brightness: int | None = Field(default=None, ge=0, le=100)
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

    return {
        "profile": "ground_station",
        "paired_drone": {
            "device_id": None,
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


@router.get("/network")
async def get_ground_station_network() -> dict[str, Any]:
    """Network uplinks view. Only AP has real data in Phase 1."""
    app = _require_ground_profile()
    return {
        "ap": _ap_view(app),
        "wifi_client": {"enabled": False, "ssid": None, "connected": False},
        "ethernet": {"enabled": False, "connected": False, "ip": None},
        "modem_4g": {"enabled": False, "connected": False, "carrier": None},
        "active_uplink": None,
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


@router.put("/ui/oled")
async def put_ground_station_ui_oled(update: OledUpdate) -> dict[str, Any]:
    """Update OLED settings. Returns the full UI config."""
    _require_ground_profile()

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
        _save_ui_config(data)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc
    return data


@router.put("/ui/buttons")
async def put_ground_station_ui_buttons(update: ButtonsUpdate) -> dict[str, Any]:
    """Replace the button mapping. Returns the full UI config.

    Phase 1 note: the OLED/button service does not consume this remap
    yet (Phase 2 work). The endpoint is live so the GCS matrix UI has
    a persistent home for the binding.
    """
    _require_ground_profile()

    data = _load_ui_config()
    if update.mapping is not None:
        data["buttons"] = {"mapping": dict(update.mapping)}

    try:
        _save_ui_config(data)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc
    return data


@router.put("/ui/screens")
async def put_ground_station_ui_screens(update: ScreensUpdate) -> dict[str, Any]:
    """Update screen order and/or enabled set. Returns the full UI config."""
    _require_ground_profile()

    data = _load_ui_config()
    screens = dict(data["screens"])
    if update.order is not None:
        screens["order"] = list(update.order)
    if update.enabled is not None:
        screens["enabled"] = list(update.enabled)
    data["screens"] = screens

    try:
        _save_ui_config(data)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_UI_SAVE_FAILED", "message": str(exc)}},
        ) from exc
    return data


# ---------------------------------------------------------------------------
# /factory-reset
# ---------------------------------------------------------------------------


@router.post("/factory-reset")
async def post_factory_reset(
    confirm: str = Query(..., description="Current pair key fingerprint or stock token"),
) -> dict[str, Any]:
    """Wipe pair state and AP passphrase. Requires the current fingerprint.

    When the ground station is paired, the confirm token must match the
    active pair key fingerprint. When unpaired, the token must match
    `factory-reset-unpaired`. This stops a casual curl from bricking a
    live device.
    """
    _require_ground_profile()

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
