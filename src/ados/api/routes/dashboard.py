"""Dashboard snapshot endpoint.

The agent webapp's one-pager polls `/api/v1/dashboard/snapshot` at 1 Hz
(slower when the tab is hidden) and reads each panel's slice from the
returned dict. The shape is intentionally flat so the JS side can pick
fields without normalisation.

Every slice here is sourced from something this process can actually read:
the vehicle state the native router publishes, the agent config, the pairing
state, and the local mediamtx paths list. A field with no source is ``None``,
never a default that reads as a measurement. The mesh, relay-source, uplink
and radio-receive views live on the native ground-station routes, which own
that data; this snapshot does not repeat them.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter

from ados.api.deps import get_agent_app
from ados.api.routes.video._common import mediamtx_ready

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


# MAVLink MAV_TYPE enum → human-facing vehicle name. The FC sends this
# in every HEARTBEAT; rendering the raw integer in the FC card read as
# "vehicle: 2" which carried zero meaning.
_MAV_TYPE_NAMES: dict[int, str] = {
    0: "Generic",
    1: "Fixed Wing",
    2: "Quadrotor",
    3: "Coaxial",
    4: "Helicopter",
    5: "Antenna Tracker",
    6: "GCS",
    7: "Airship",
    8: "Free Balloon",
    9: "Rocket",
    10: "Ground Rover",
    11: "Surface Boat",
    12: "Submarine",
    13: "Hexarotor",
    14: "Octorotor",
    15: "Tricopter",
    16: "Flapping Wing",
    17: "Kite",
    18: "Onboard Controller",
    19: "VTOL Tailsitter Duo",
    20: "VTOL Quadrotor",
    21: "VTOL Tiltrotor",
    22: "VTOL Reserved",
    23: "VTOL Reserved",
    24: "VTOL Reserved",
    25: "VTOL Reserved",
    26: "VTOL Reserved",
    27: "VTOL Tailsitter",
    28: "VTOL Tiltwing",
    29: "VTOL Reserved",
    30: "Gimbal",
    31: "ADSB",
    32: "Parafoil",
    33: "Dodecarotor",
    34: "Camera",
    35: "Charging Station",
    36: "FLARM",
    37: "Servo",
    38: "ODID",
    39: "Decarotor",
    40: "Battery",
    41: "Parachute",
}


def _mav_type_name(value: Any) -> str | None:
    """Map the MAVLink MAV_TYPE enum id to a human label."""
    if value is None:
        return None
    try:
        ident = int(value)
    except (TypeError, ValueError):
        text = str(value).strip()
        return text or None
    return _MAV_TYPE_NAMES.get(ident, f"vehicle {ident}")


def _normalize_rc_rssi(value: Any) -> int | None:
    """Normalize raw MAVLink RC RSSI byte (0-254) to a 0-100% scale.

    Some FC firmware emit the rssi field directly as a percent in the
    [0, 100] range; ArduPilot fills the standard 0-254 byte where 254
    means "max signal". Values <= 100 pass through; anything above is
    scaled by 100 / 254 and clamped to [0, 100].
    """
    if value is None:
        return None
    try:
        raw = float(value)
    except (TypeError, ValueError):
        return None
    if raw <= 0:
        return 0
    if raw <= 100:
        return int(round(raw))
    pct = round(raw * 100 / 254)
    return max(0, min(100, int(pct)))


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


def _video_slice(
    app: Any, mediamtx_is_ready: bool, track_info: dict[str, Any] | None
) -> dict[str, Any]:
    """The video panel slice from the configured encoder and the live stream.

    ``codec``/``width``/``height``/``fps`` and ``target_bitrate_kbps`` are the
    configured encoder settings. ``bitrate_kbps`` is only ever the rate measured
    off mediamtx's received-bytes counter; it is ``None`` until two readings of
    a live stream exist, so a configured target never reads as a live rate.

    Readiness is decided by the authoritative paths list (a publisher is really
    delivering only when the ``main`` path is ready WITH a source), passed in by
    the caller so this stays a pure projection.
    """
    state: dict[str, Any] = {}
    cfg = _safe(lambda: app.config.video, None)
    if cfg is not None:
        state.update(
            {
                "codec": _safe(lambda: str(cfg.encoder.codec or ""), ""),
                "width": _safe(lambda: int(cfg.encoder.width or 0), 0),
                "height": _safe(lambda: int(cfg.encoder.height or 0), 0),
                "fps": _safe(lambda: int(cfg.encoder.fps or 0), 0),
                "target_bitrate_kbps": _safe(
                    lambda: int(cfg.encoder.bitrate_kbps or 0) or None, None
                ),
            }
        )

    if mediamtx_is_ready:
        state["state"] = "running"
    elif _video_devices_present():
        state["state"] = "ready"
    else:
        state["state"] = "no_camera"

    state["bitrate_kbps"] = None
    if mediamtx_is_ready and track_info:
        live_codec = track_info.get("codec")
        if isinstance(live_codec, str) and live_codec.strip():
            state["codec"] = live_codec.strip()
        state["bitrate_kbps"] = _bitrate_from_mediamtx(track_info)
    else:
        # The stream is gone: a later reading must not be diffed against a
        # byte count from a publisher that no longer exists.
        _BITRATE_SAMPLE.pop("main", None)

    state["glass_to_glass_ms"] = None
    return state


# Module-level last-sample cache for the MediaMTX bytes counter so we
# can turn a monotonically-increasing total into a rolling kbps. Keyed
# by path name (we only publish ``main`` today). Survives across
# /api/v1/dashboard/snapshot polls as long as the API process is up.
_BITRATE_SAMPLE: dict[str, tuple[float, int]] = {}


def _bitrate_from_mediamtx(track_info: dict[str, Any]) -> int | None:
    """Convert MediaMTX bytesReceived deltas into a rolling kbps."""
    bytes_received = track_info.get("bytes_received")
    if not isinstance(bytes_received, int) or bytes_received < 0:
        return None
    now = time.monotonic()
    last = _BITRATE_SAMPLE.get("main")
    _BITRATE_SAMPLE["main"] = (now, bytes_received)
    if last is None:
        return None
    last_ts, last_bytes = last
    elapsed = now - last_ts
    if elapsed <= 0.0 or bytes_received < last_bytes:
        # Reset (counter rewound on stream restart) — wait for the next
        # sample to produce a delta.
        return None
    delta_bytes = bytes_received - last_bytes
    kbps = round((delta_bytes * 8) / 1000 / elapsed)
    return int(max(0, kbps))


# GPS_RAW_INT sends eph as HDOP x100 with 65535 meaning "unknown"; the router
# publishes it divided by 100, so the unknown sentinel arrives as 655.35.
_HDOP_UNKNOWN = 655.35


def _hdop(eph: Any) -> float | None:
    """HDOP from the router's ``gps.eph``, or None when the FC did not know it."""
    if isinstance(eph, bool) or not isinstance(eph, (int, float)):
        return None
    if eph <= 0 or eph >= _HDOP_UNKNOWN:
        return None
    return float(eph)


def _fc_slice(app: Any) -> dict[str, Any]:
    """The FC panel slice, read from the keys the native router publishes.

    The router's vehicle snapshot is zero-initialised: ``armed`` is ``false``
    and ``mode`` is empty until the first HEARTBEAT lands, and the GPS block
    reads 0 until the first GPS_RAW_INT. Those defaults are not readings, so
    every heartbeat-derived field is ``None`` until ``last_heartbeat`` is set.
    """
    fc = _safe(lambda: app.fc_status(), None)
    veh = _safe(lambda: app.vehicle_state_dict(), {}) or {}
    if fc is None:
        return {}
    gps = veh.get("gps") if isinstance(veh.get("gps"), dict) else {}
    battery = veh.get("battery") if isinstance(veh.get("battery"), dict) else {}
    rc_block = veh.get("rc") if isinstance(veh.get("rc"), dict) else {}
    last_heartbeat = veh.get("last_heartbeat") or None
    heard = last_heartbeat is not None
    autopilot_raw = veh.get("autopilot") if heard else None
    mav_type_raw = veh.get("mav_type") if heard else None
    armed = veh.get("armed")
    fix_type = gps.get("fix_type")
    # No GPS message yet (fix_type 0 = NO_GPS) leaves satellites at the
    # zero default; only a reported fix makes the count a reading.
    has_gps = isinstance(fix_type, int) and not isinstance(fix_type, bool) and fix_type > 0
    return {
        "vehicle": _mav_type_name(mav_type_raw),
        "vehicle_id": (int(mav_type_raw) if isinstance(mav_type_raw, (int, float)) else None),
        "firmware": _autopilot_name(autopilot_raw),
        "firmware_id": (int(autopilot_raw) if isinstance(autopilot_raw, (int, float)) else None),
        "mode": (str(veh.get("mode")) if heard and veh.get("mode") else None),
        # Only what a heartbeat said. An FC that has not reported is not
        # "disarmed"; it is unknown.
        "armed": armed if heard and isinstance(armed, bool) else None,
        "gps": {
            "fix_type": fix_type if has_gps else None,
            "satellites_visible": gps.get("satellites") if has_gps else None,
            "hdop": _hdop(gps.get("eph")) if has_gps else None,
        },
        "battery": {
            "voltage": battery.get("voltage"),
            "remaining": battery.get("remaining"),
        },
        "rc": _normalize_rc_rssi(rc_block.get("rssi")),
        "fc_port": fc.port,
        "fc_baud": fc.baud,
        "connected": fc.connected,
        "last_heartbeat": last_heartbeat,
    }


def _cloud_slice(app: Any) -> dict[str, Any]:
    """The cloud posture from config plus the live pairing code.

    The relay's own link state is held by the native cloud service, not
    this process, so ``mqtt_state`` / ``http_state`` / ``rtt_ms`` are
    ``None`` (unknown) rather than a placeholder string that reads as a state.
    """
    cfg = _safe(lambda: app.config, None)
    drone_id = ""
    mode = None
    if cfg is not None:
        drone_id = _safe(lambda: str(cfg.agent.device_id or ""), "")
        mode = _safe(lambda: str(cfg.server.mode), None)
    pm = _safe(lambda: app.pairing_manager, None)
    pairing_code = ""
    if pm is not None and not _safe(lambda: bool(pm.is_paired), True):
        pairing_code = _safe(lambda: str(pm.get_or_create_code() or ""), "")
    return {
        "mode": mode,
        "mqtt_state": None,
        "http_state": None,
        "rtt_ms": None,
        "drone_id": drone_id,
        "pairing_code": pairing_code,
    }


@router.get("/snapshot")
async def get_dashboard_snapshot() -> dict[str, Any]:
    """Combined dashboard snapshot. 1 Hz polling target."""
    app = get_agent_app()
    ready, track_info = await mediamtx_ready()
    return {
        "video": _video_slice(app, ready, track_info),
        "fc": _fc_slice(app),
        "cloud": _cloud_slice(app),
    }
