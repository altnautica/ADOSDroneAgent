"""Shared helpers, Pydantic models, and constants for ground-station routes.

Every sub-router module imports these helpers indirectly. Tests
monkeypatch them through `ados.api.routes.ground_station.<name>`,
which is wired via the package `__init__.py`. Sub-modules read the
helper at call time through the package object so monkeypatched
values take effect at the call site.
"""

from __future__ import annotations

import json
import re as _re
import time
from pathlib import Path
from typing import Any, Literal

from fastapi import HTTPException
from pydantic import BaseModel, Field, field_validator

from ados.api.deps import get_agent_app
from ados.core.paths import (
    GS_UI_JSON,
    GS_UPLINK_JSON,
    MESH_GATEWAY_JSON,
    MESH_ID_PATH,
    MESH_STATE_JSON,
    PROFILE_CONF,
    WFB_RECEIVER_JSON,
    WFB_RELAY_JSON,
)


# Persistent UI config lives in a separate JSON file because the
# current Pydantic ADOSConfig does not model a ground_station section
# yet. Single file, atomic write, 0644.
_UI_CONFIG_PATH = GS_UI_JSON

# Server and OLED both use 0-255 native scale. 204 is roughly 80 percent
# of 255.
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

_DEFAULT_DISPLAY: dict[str, Any] = {
    "resolution": "auto",
    "kiosk_enabled": False,
    "kiosk_target_url": None,
}

_UPLINK_PRIORITY_PATH = GS_UPLINK_JSON

_MESH_STATE_JSON = MESH_STATE_JSON
_WFB_RELAY_JSON = WFB_RELAY_JSON
_WFB_RECEIVER_JSON = WFB_RECEIVER_JSON

_IPV4_RE = _re.compile(
    r"^((25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$"
)


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
# Pydantic request models
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

    Server and OLED both use the 0-255 native scale. This matches
    luma.oled device.contrast() directly and the GCS slider range.
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


class RoleChangeRequest(BaseModel):
    role: Literal["direct", "relay", "receiver"]
    confirm_token: str | None = None


class MeshConfigUpdate(BaseModel):
    mesh_id: str | None = None
    carrier: Literal["802.11s", "ibss"] | None = None
    channel: int | None = Field(default=None, ge=1, le=13)


class MeshGatewayPreferenceUpdate(BaseModel):
    mode: Literal["auto", "pinned", "off"]
    pinned_mac: str | None = None


class PairAcceptRequest(BaseModel):
    duration_s: int = Field(default=60, ge=5, le=300)


class PairApproveRequest(BaseModel):
    device_id: str


class PairRevokeRequest(BaseModel):
    device_id: str


class PairJoinRequest(BaseModel):
    receiver_host: str | None = None
    receiver_port: int | None = Field(default=None, ge=1, le=65535)


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


# ---------------------------------------------------------------------------
# View helpers
# ---------------------------------------------------------------------------


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


def _link_view(app: Any) -> dict[str, Any]:
    """Best-effort link view. Channel comes from config; the rest is stubbed."""
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


def _read_wfb_view(app: Any) -> dict[str, Any]:
    wfb_cfg = getattr(app.config, "wfb", None)
    return {
        "channel": getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0,
        "bitrate_profile": getattr(wfb_cfg, "bitrate_profile", "default")
        if wfb_cfg is not None
        else "default",
        "fec": getattr(wfb_cfg, "fec", "8/12") if wfb_cfg is not None else "8/12",
    }


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
    """Surface WifiClientManager status + enabled_on_boot."""
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

    Authoritative source is `ADOSConfig.ground_station.share_uplink`
    (YAML). The legacy JSON side-file at `_UI_CONFIG_PATH` is handled by
    the one-shot migrator in `ados.core.config.load_config()` and
    preserved on disk.
    """
    try:
        from ados.core.config import load_config

        cfg = load_config()
        return bool(cfg.ground_station.share_uplink)
    except Exception:
        return False


def _ethernet_mgr() -> Any:
    from ados.services.ground_station.ethernet_manager import (
        get_ethernet_manager,
    )

    return get_ethernet_manager()


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


def _input_manager() -> Any:
    """Lazy import helper for the InputManager singleton."""
    from ados.services.ground_station.input_manager import get_input_manager

    return get_input_manager()


def _pic_arbiter() -> Any:
    """Lazy import helper for the PicArbiter singleton."""
    from ados.services.ground_station.pic_arbiter import get_pic_arbiter

    return get_pic_arbiter()


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


def _persist_gs_ui_section(section: str, value: dict[str, Any]) -> None:
    """Write `ground_station.ui.<section>` into the YAML-backed ADOSConfig.

    The OLED, button, and screen UI config lives in the Pydantic model
    so it round-trips through save cycles and is consumed by the live
    services. The legacy JSON side-file is no longer written, but remains
    on disk for rollback (the load-time migrator preserves it).
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


def _persist_share_uplink_flag(enabled: bool) -> None:
    """Write share_uplink into the Pydantic-backed ADOSConfig on disk.

    Writes to `/etc/ados/config.yaml` under `ground_station.share_uplink`.
    The legacy JSON side-file is not written but is preserved on disk
    for rollback. The pair_manager atomic save helper is reused so air
    and ground paths share one code path.
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

    Delegates to `services/ground_station/share_uplink_firewall.apply_share_uplink`
    which handles distro detection, iptables-persistent vs nftables
    fallback, atomic sysctl drop-in, and persistence of the rule set.
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


def _read_json_or_empty(path: Path) -> dict[str, Any]:
    try:
        if path.is_file():
            return json.loads(path.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        pass
    return {}


def _read_yaml_or_empty(path: Path) -> dict[str, Any]:
    """Read a YAML file into a dict. Returns {} on any failure.

    Used for `/etc/ados/profile.conf` which is written as YAML by
    `profile_detect.write_profile_conf` and by the ground-station install
    path.
    """
    try:
        if path.is_file():
            import yaml as _yaml
            data = _yaml.safe_load(path.read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        pass
    except Exception:
        # Corrupt YAML should not crash the endpoint; treat as empty.
        pass
    return {}
