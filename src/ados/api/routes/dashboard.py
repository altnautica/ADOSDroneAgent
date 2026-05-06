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

from typing import Any

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter(prefix="/v1/dashboard", tags=["dashboard"])


def _safe(fn: Any, default: Any) -> Any:
    try:
        return fn()
    except Exception:
        return default


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
    state.setdefault("state", "unknown")
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
    return {
        "vehicle": str(veh.get("vehicle_type") or ""),
        "firmware": str(veh.get("autopilot") or ""),
        "mode": str(veh.get("flight_mode") or veh.get("mode") or ""),
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
        "rc": veh.get("rc"),
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
    if cfg is not None:
        drone_id = _safe(lambda: str(cfg.agent.device_id or ""), "")
        pairing_code = _safe(lambda: str(cfg.cloud.pairing_code or ""), "")
    return {
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
    }
