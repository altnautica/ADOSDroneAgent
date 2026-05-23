"""Dashboard snapshot endpoint.

The agent webapp's one-pager polls `/api/v1/dashboard/snapshot` at 1 Hz
(slower when the tab is hidden) and reads each panel's slice from the
returned dict. The shape is intentionally flat so the JS side can pick
fields without normalisation.

Field availability is best-effort. The endpoint never raises on a
missing subsystem, so the dashboard renders a partial snapshot instead
of a blank page when one upstream is misbehaving.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/dashboard", tags=["dashboard"])


def _safe(fn: Any, default: Any) -> Any:
    try:
        return fn()
    except Exception:
        return default


# MAVLink MAV_AUTOPILOT enum → operator-facing firmware label. The
# numeric ids land in the heartbeat from any FC; rendering the raw int
# in the dashboard read as "firmware: 3" which carries zero meaning.
_AUTOPILOT_NAMES: dict[int, str] = {
    0: "Generic",
    3: "ArduPilot",
    4: "OpenPilot",
    5: "Generic Waypoints Only",
    6: "Generic Waypoints + Simple Nav",
    7: "Generic Full Mission",
    8: "Invalid",
    9: "PPZ",
    10: "UDB",
    11: "FP",
    12: "PX4",
    13: "SMACCM",
    14: "AutoQuad",
    15: "Armazila",
    16: "Aerob",
    17: "ASLUAV",
    18: "SmartAP",
    19: "AirRails",
    20: "ReflectronUDP",
}


def _autopilot_name(value: Any) -> str | None:
    """Map the MAVLink autopilot enum id to a human label."""
    if value is None:
        return None
    try:
        ident = int(value)
    except (TypeError, ValueError):
        text = str(value).strip()
        return text or None
    return _AUTOPILOT_NAMES.get(ident, f"autopilot {ident}")


def _video_devices_present() -> bool:
    """Cheap kernel-side check: any V4L2 node enumerated?

    Used by the dashboard snapshot (polled at 1 Hz) to distinguish
    ``no_camera`` from ``ready`` without paying for a full HAL camera
    discovery on every tick. The rich camera detail still comes from
    ``hardware_check`` in the setup-status path.
    """
    try:
        return any(Path("/sys/class/video4linux").iterdir())
    except OSError:
        return False


def _video_slice(app: Any) -> dict[str, Any]:
    state: dict[str, Any] = {}
    try:
        cfg = app.config.video
    except Exception:
        cfg = None
    if cfg is not None:
        state.update(
            {
                "codec": _safe(lambda: str(cfg.encoder.codec or ""), ""),
                "width": _safe(lambda: int(cfg.encoder.width or 0), 0),
                "height": _safe(lambda: int(cfg.encoder.height or 0), 0),
                "fps": _safe(lambda: int(cfg.encoder.fps or 0), 0),
                "bitrate_kbps": _safe(lambda: int(cfg.encoder.bitrate_kbps or 0), 0),
            }
        )

    mediamtx_alive = False
    track_info: dict[str, Any] | None = None
    try:
        from ados.api.routes.video._common import (
            mediamtx_track_info_sync,
            mediamtx_whep_alive_sync,
        )

        mediamtx_alive = mediamtx_whep_alive_sync()
        if mediamtx_alive:
            track_info = mediamtx_track_info_sync()
    except Exception:
        mediamtx_alive = False
        track_info = None

    if mediamtx_alive:
        state["state"] = "running"
    elif _video_devices_present():
        state["state"] = "ready"
    else:
        state["state"] = "no_camera"

    # Prefer live MediaMTX track info over static encoder config when
    # the stream is actually flowing. The encoder block stays
    # authoritative when MediaMTX is silent (e.g. no clients have
    # pulled the stream yet).
    if track_info:
        live_codec = track_info.get("codec")
        if isinstance(live_codec, str) and live_codec.strip():
            state["codec"] = live_codec.strip()

    state.setdefault("glass_to_glass_ms", None)
    return state


def _fc_slice(app: Any) -> dict[str, Any]:
    fc = _safe(lambda: app.fc_status(), None)
    veh = _safe(lambda: app.vehicle_state_dict(), {}) or {}
    if fc is None:
        return {}
    gps = veh.get("gps") if isinstance(veh, dict) else {}
    if not isinstance(gps, dict):
        gps = {}
    battery = veh.get("battery") if isinstance(veh, dict) else {}
    if not isinstance(battery, dict):
        battery = {}
    # Return None for missing string fields so the dashboard can
    # distinguish "FC connected but waiting for telemetry" from
    # "FC reported empty values". Empty strings render as visible
    # blanks in the UI rather than triggering the "—" fallback.
    rc_block = veh.get("rc") if isinstance(veh.get("rc"), dict) else {}
    rc_rssi = rc_block.get("rssi") if isinstance(rc_block, dict) else None
    autopilot_raw = veh.get("autopilot")
    return {
        "vehicle": (str(veh.get("vehicle_type")) if veh.get("vehicle_type") else None),
        "firmware": _autopilot_name(autopilot_raw),
        "firmware_id": (int(autopilot_raw) if isinstance(autopilot_raw, (int, float)) else None),
        "mode": (
            str(veh.get("flight_mode") or veh.get("mode"))
            if (veh.get("flight_mode") or veh.get("mode"))
            else None
        ),
        "armed": bool(veh.get("armed", False)),
        "gps": {
            "fix_type": gps.get("fix_type"),
            "satellites_visible": gps.get("satellites_visible"),
            "hdop": gps.get("hdop"),
        },
        "battery": {
            "voltage": battery.get("voltage"),
            "remaining": battery.get("remaining"),
        },
        "link_quality": veh.get("link_quality"),
        "rc": rc_rssi if isinstance(rc_rssi, (int, float)) else None,
        "prearm": veh.get("prearm"),
        "fc_port": fc.port,
        "fc_baud": fc.baud,
        "connected": fc.connected,
        "last_heartbeat": veh.get("last_heartbeat"),
    }


def _mavlink_rates_slice(app: Any) -> dict[str, Any]:
    veh = _safe(lambda: app.vehicle_state_dict(), {}) or {}
    rates = veh.get("message_rates") if isinstance(veh, dict) else {}
    if not isinstance(rates, dict):
        return {}
    out: dict[str, Any] = {}
    for name, value in rates.items():
        if isinstance(value, dict):
            out[name] = {
                "hz": value.get("hz"),
                "last_ms": value.get("last_ms"),
            }
        elif isinstance(value, (int, float)):
            out[name] = {"hz": float(value), "last_ms": None}
    return out


def _camera_slice(app: Any) -> dict[str, Any]:
    cfg = _safe(lambda: app.config.video, None)
    if cfg is None:
        return {}
    return {
        "device": _safe(lambda: str(cfg.source.device or ""), ""),
        "codec": _safe(lambda: str(cfg.encoder.codec or ""), ""),
        "width": _safe(lambda: int(cfg.encoder.width or 0), 0),
        "height": _safe(lambda: int(cfg.encoder.height or 0), 0),
        "fps": _safe(lambda: int(cfg.encoder.fps or 0), 0),
        "bitrate_kbps": _safe(lambda: int(cfg.encoder.bitrate_kbps or 0), 0),
        "encoder_api": _safe(lambda: str(cfg.encoder.encoder or ""), ""),
        "state": "unknown",
        "dropped_frames": None,
        "encoder_cpu_pct": None,
    }


def _sensors_slice(app: Any) -> list[dict[str, Any]]:
    veh = _safe(lambda: app.vehicle_state_dict(), {}) or {}
    sensors = veh.get("sensors") if isinstance(veh, dict) else None
    if isinstance(sensors, list):
        return [s for s in sensors if isinstance(s, dict)]
    return []


def _plugins_slice(app: Any) -> list[dict[str, Any]]:
    plugins = _safe(lambda: app.plugin_state_summary(), None)
    if not isinstance(plugins, list):
        return []
    return [
        {
            "id": str(p.get("id", "")),
            "name": str(p.get("name", p.get("id", ""))),
            "state": str(p.get("state", "unknown")),
            "capabilities": list(p.get("capabilities", []) or []),
        }
        for p in plugins
        if isinstance(p, dict)
    ]


def _cloud_slice(app: Any) -> dict[str, Any]:
    cfg = _safe(lambda: app.config, None)
    cloud_state = _safe(lambda: app.cloud_relay_summary(), None) or {}
    drone_id = ""
    pairing_code = ""
    mode = "local"
    if cfg is not None:
        drone_id = _safe(lambda: str(cfg.agent.device_id or ""), "")
        pairing_code = _safe(lambda: str(cfg.cloud.pairing_code or ""), "")
        mode = _safe(lambda: str(cfg.server.mode or "local"), "local")
    return {
        "mode": mode,
        "mqtt_state": cloud_state.get("mqtt_state", "unknown"),
        "http_state": cloud_state.get("http_state", "unknown"),
        "rtt_ms": cloud_state.get("rtt_ms"),
        "drone_id": drone_id,
        "pairing_code": pairing_code,
    }


def _network_slice(app: Any) -> dict[str, Any]:
    summary = _safe(lambda: app.network_summary(), None)
    if isinstance(summary, dict):
        return summary
    return {}


def _wfb_rx_slice(app: Any) -> dict[str, Any]:
    """WFB receive stats. Best-effort pull from wfb_summary() and the
    ground_station + video.wfb config blocks. Streams default to an
    empty list when the runtime hasn't populated them yet."""
    summary = _safe(lambda: app.wfb_summary(), None)
    if isinstance(summary, dict) and summary:
        out = dict(summary)
    else:
        out = {}

    cfg = _safe(lambda: app.config, None)
    if cfg is not None:
        wfb_cfg = _safe(lambda: cfg.video.wfb, None)
        if wfb_cfg is not None:
            out.setdefault("adapter", _safe(lambda: str(wfb_cfg.interface or ""), ""))
            out.setdefault("channel", _safe(lambda: int(wfb_cfg.channel or 0), 0))
        gs_cfg = _safe(lambda: cfg.ground_station, None)
        if gs_cfg is not None:
            # Prefer the ground-station-side adapter override if the
            # runtime exposed one, otherwise fall back to the air-side
            # WfbConfig.interface field already set above.
            iface_override = _safe(lambda: getattr(gs_cfg, "rx_interface", None), None)
            if iface_override:
                out["adapter"] = str(iface_override)

    out.setdefault("freq_mhz", None)
    out.setdefault("rssi_dbm", None)
    out.setdefault("packet_loss_pct", None)
    out.setdefault("fec_recovered", None)
    out.setdefault("fec_failed", None)
    out.setdefault("bitrate_kbps", None)
    streams = out.get("streams")
    if not isinstance(streams, list):
        out["streams"] = []
    return out


def _mesh_slice(app: Any) -> dict[str, Any]:
    """Local batman-adv mesh state. Role drives the ground panel filter,
    so the role from config is the authoritative fallback when the
    runtime helper is missing."""
    summary = _safe(lambda: app.mesh_summary(), None)
    if isinstance(summary, dict) and summary:
        out = dict(summary)
    else:
        out = {}

    cfg = _safe(lambda: app.config, None)
    if cfg is not None:
        gs_cfg = _safe(lambda: cfg.ground_station, None)
        if gs_cfg is not None:
            out.setdefault("role", _safe(lambda: str(gs_cfg.role or "direct"), "direct"))

    out.setdefault("role", "direct")
    peers = out.get("batman_peers")
    if not isinstance(peers, list):
        out["batman_peers"] = []
    out.setdefault("gateway_node", None)
    out.setdefault("partition_state", None)
    out.setdefault("mesh_addr", None)
    return out


def _sources_slice(app: Any) -> dict[str, Any]:
    """Aggregated stream-source stats. Receiver-only; the panel checks
    the role itself, so we always return the dict shape."""
    summary = _safe(lambda: app.wfb_summary(), None)
    if isinstance(summary, dict):
        candidate = summary.get("sources")
        if isinstance(candidate, dict):
            out = dict(candidate)
        else:
            out = {}
    else:
        out = {}
    out.setdefault("aggregated_kbps", None)
    out.setdefault("frames_combined", None)
    out.setdefault("frames_dedup", None)
    per = out.get("per_source")
    if not isinstance(per, list):
        out["per_source"] = []
    return out


def _display_slice(app: Any) -> dict[str, Any]:
    """Local kiosk / HDMI display state. Pulled from the optional
    `app.config.display` block when present."""
    cfg = _safe(lambda: app.config, None)
    if cfg is None:
        return {}
    disp = _safe(lambda: getattr(cfg, "display", None), None)
    if disp is None:
        return {}
    return {
        "device": _safe(lambda: str(getattr(disp, "device", "") or ""), ""),
        "kiosk_url": _safe(lambda: str(getattr(disp, "kiosk_url", "") or ""), ""),
        "width": _safe(lambda: int(getattr(disp, "width", 0) or 0), 0),
        "height": _safe(lambda: int(getattr(disp, "height", 0) or 0), 0),
        "refresh_hz": _safe(lambda: int(getattr(disp, "refresh_hz", 0) or 0), 0),
        "content": _safe(lambda: str(getattr(disp, "content", "") or ""), ""),
    }


def _peripheral_dict(app: Any) -> dict[str, Any]:
    summary = _safe(lambda: app.peripheral_summary(), None)
    if isinstance(summary, dict):
        return summary
    return {}


def _oled_slice(app: Any) -> dict[str, Any]:
    peri = _peripheral_dict(app)
    oled = peri.get("oled") if isinstance(peri, dict) else None
    if not isinstance(oled, dict):
        return {}
    return {
        "screen": oled.get("screen"),
        "brightness": oled.get("brightness"),
        "contrast": oled.get("contrast"),
    }


def _buttons_slice(app: Any) -> dict[str, Any]:
    peri = _peripheral_dict(app)
    btn = peri.get("buttons") if isinstance(peri, dict) else None
    if not isinstance(btn, dict):
        # The button mapping also lives on the ground_station ui config
        # block; surface it from there when the runtime helper is silent.
        cfg = _safe(lambda: app.config, None)
        if cfg is not None:
            ui = _safe(lambda: cfg.ground_station.ui, None)
            if ui is not None:
                mapping = _safe(lambda: dict(ui.buttons or {}), {}) or {}
                return {"mapping": mapping, "last_event": None}
        return {}
    mapping = btn.get("mapping")
    if not isinstance(mapping, dict):
        mapping = {}
    last = btn.get("last_event")
    if not isinstance(last, dict):
        last = None
    return {"mapping": mapping, "last_event": last}


def _joystick_slice(app: Any) -> dict[str, Any]:
    peri = _peripheral_dict(app)
    js = peri.get("joystick") if isinstance(peri, dict) else None
    if not isinstance(js, dict):
        return {}
    axes = js.get("axes") if isinstance(js.get("axes"), list) else []
    buttons = js.get("buttons") if isinstance(js.get("buttons"), list) else []
    return {
        "device": js.get("device"),
        "vendor": js.get("vendor"),
        "product": js.get("product"),
        "axes": [a for a in axes if isinstance(a, dict)],
        "buttons": [b for b in buttons if isinstance(b, dict)],
    }


@router.get("/snapshot")
async def get_dashboard_snapshot() -> dict[str, Any]:
    """Combined dashboard snapshot. 1 Hz polling target."""
    app = get_agent_app()
    return {
        "video": _video_slice(app),
        "fc": _fc_slice(app),
        "mavlink_rates": _mavlink_rates_slice(app),
        "camera": _camera_slice(app),
        "sensors": _sensors_slice(app),
        "plugins": _plugins_slice(app),
        "cloud": _cloud_slice(app),
        "network": _network_slice(app),
        "wfb_rx": _wfb_rx_slice(app),
        "mesh": _mesh_slice(app),
        "sources": _sources_slice(app),
        "display": _display_slice(app),
        "oled": _oled_slice(app),
        "buttons": _buttons_slice(app),
        "joystick": _joystick_slice(app),
    }
