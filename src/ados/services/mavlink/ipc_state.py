"""Read-only views over the MAVLink router's state IPC snapshot.

The native router owns the FC link and publishes a vehicle-state snapshot
to ``/run/ados/state.sock`` at ~10 Hz (newline-JSON v1 / length-prefixed
msgpack v2, decoded by :class:`ados.core.ipc.StateIPCClient`). The snapshot
carries the vehicle dict (heartbeat, attitude, gps, battery, rc, ...) plus a
set of service extras (``fc_connected``, ``fc_port``, ``fc_baud``,
``service_uptime``, the param-sweep flags, and the ``params`` blob).

These shims wrap that snapshot dict and expose the small attribute surface
the API layer, the MQTT gateway, and the cloud heartbeat already expect from
the former in-process objects (``.connected`` / ``.port`` / ``.baud`` on the
FC handle, ``.to_dict()`` / ``.armed`` / ``.params`` on the vehicle state,
``.get_all()`` / ``.get()`` / ``.count`` on the param cache). They are
passive: feed them with :meth:`IpcVehicleState.update_from_dict` from a
``StateIPCClient`` state handler, or share a single :class:`IpcVehicleState`
across all three so the param cache and FC handle read the same snapshot.
"""

from __future__ import annotations

# Service extras the router rides alongside the vehicle keys. Stripped from
# ``to_dict()`` so the telemetry surface returns only vehicle-state fields.
_EXTRA_KEYS = frozenset(
    {
        "fc_connected",
        "fc_port",
        "fc_baud",
        "service_uptime",
        "param_priming",
        "param_sweep_timed_out",
        "param_sweep_send_failed",
        "param_cached_count",
        "param_expected_count",
        "params",
    }
)


def _empty_vehicle_dict() -> dict:
    """Default vehicle dict matching the router's snapshot shape when empty."""
    return {
        "mav_type": 0,
        "autopilot": 0,
        "armed": False,
        "mode": "",
        "position": {
            "lat": 0.0,
            "lon": 0.0,
            "alt_msl": 0.0,
            "alt_rel": 0.0,
            "heading": 0.0,
        },
        "velocity": {
            "vx": 0.0,
            "vy": 0.0,
            "vz": 0.0,
            "groundspeed": 0.0,
            "airspeed": 0.0,
            "climb": 0.0,
        },
        "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 0.0},
        "battery": {
            "voltage": 0.0,
            "current": 0.0,
            "remaining": -1,
            "temperature": 0.0,
            "cell_voltages": [],
        },
        "gps": {"fix_type": 0, "satellites": 0, "eph": 0.0, "epv": 0.0},
        "rc": {"channels": [0] * 18, "rssi": 0},
        "throttle": 0,
        "last_heartbeat": "",
        "last_update": "",
    }


class IpcVehicleState:
    """Passive vehicle-state view fed from the router's state IPC snapshot.

    Holds the most recent snapshot dict and exposes the attribute surface
    the former in-process ``VehicleState`` offered to its readers.
    """

    def __init__(self) -> None:
        self._d: dict = {}

    def update_from_dict(self, d: dict) -> None:
        """Replace the held snapshot with a fresh state IPC dict."""
        if d:
            self._d = dict(d)

    @property
    def snapshot(self) -> dict:
        """The raw held snapshot (vehicle keys + service extras)."""
        return self._d

    @property
    def armed(self) -> bool:
        return bool(self._d.get("armed", False))

    @property
    def mode(self) -> str:
        return str(self._d.get("mode", "") or "")

    @property
    def mav_type(self) -> int:
        return int(self._d.get("mav_type", 0) or 0)

    @property
    def autopilot(self) -> int:
        return int(self._d.get("autopilot", 0) or 0)

    @property
    def last_heartbeat(self) -> str:
        return str(self._d.get("last_heartbeat", "") or "")

    @property
    def last_update(self) -> str:
        return str(self._d.get("last_update", "") or "")

    def _nested(self, group: str, key: str, default: float = 0.0) -> float:
        sub = self._d.get(group)
        if isinstance(sub, dict):
            value = sub.get(key, default)
            return value if value is not None else default
        return default

    # Flat convenience accessors over the nested snapshot, matching the
    # attribute names the former in-process state object exposed.
    @property
    def lat(self) -> float:
        return float(self._nested("position", "lat"))

    @property
    def lon(self) -> float:
        return float(self._nested("position", "lon"))

    @property
    def alt_msl(self) -> float:
        return float(self._nested("position", "alt_msl"))

    @property
    def alt_rel(self) -> float:
        return float(self._nested("position", "alt_rel"))

    @property
    def heading(self) -> float:
        return float(self._nested("position", "heading"))

    @property
    def groundspeed(self) -> float:
        return float(self._nested("velocity", "groundspeed"))

    @property
    def airspeed(self) -> float:
        return float(self._nested("velocity", "airspeed"))

    @property
    def voltage_battery(self) -> float:
        return float(self._nested("battery", "voltage"))

    @property
    def current_battery(self) -> float:
        return float(self._nested("battery", "current"))

    @property
    def battery_remaining(self) -> int:
        return int(self._nested("battery", "remaining", -1))

    @property
    def params(self) -> dict[str, float]:
        blob = self._d.get("params")
        return dict(blob) if isinstance(blob, dict) else {}

    @property
    def param_count(self) -> int:
        return int(self._d.get("param_expected_count", 0) or 0)

    def to_dict(self) -> dict:
        """Vehicle-state dict (service extras stripped)."""
        if not self._d:
            return _empty_vehicle_dict()
        return {k: v for k, v in self._d.items() if k not in _EXTRA_KEYS}


class _ParamEntry:
    """Lightweight param record matching the persistent cache entry shape."""

    __slots__ = ("value", "param_type", "last_updated")

    def __init__(self, value: float, param_type: int = 0, last_updated: float = 0.0) -> None:
        self.value = value
        self.param_type = param_type
        self.last_updated = last_updated


class IpcParamCache:
    """Param-cache view over the router snapshot's ``params`` blob.

    The blob carries name → value only, so type metadata reads as 0 (which
    ArduPilot accepts and resolves from its canonical type table on write).
    """

    def __init__(self, vehicle_state: IpcVehicleState) -> None:
        self._vs = vehicle_state

    def get(self, name: str) -> float | None:
        value = self._vs.params.get(name)
        if value is None:
            return None
        try:
            return float(value)
        except (TypeError, ValueError):
            return None

    def get_all(self) -> dict[str, float]:
        return self._vs.params

    def get_all_detailed(self) -> dict[str, dict]:
        return {
            name: {"value": value, "param_type": 0, "last_updated": 0.0}
            for name, value in self._vs.params.items()
        }

    @property
    def count(self) -> int:
        cached = self._vs.snapshot.get("param_cached_count")
        if isinstance(cached, (int, float)):
            return int(cached)
        return len(self._vs.params)

    @property
    def _params(self) -> dict[str, _ParamEntry]:
        return {name: _ParamEntry(value) for name, value in self._vs.params.items()}


class IpcFcConnection:
    """FC-handle view exposing the link status the router publishes.

    There is no live pymavlink connection here (the router owns the FC), so
    ``connection`` stays ``None``; callers that need to send to the FC write
    frames to ``/run/ados/mavlink.sock`` via a ``MavlinkIPCClient`` instead.
    """

    connection = None

    def __init__(self, vehicle_state: IpcVehicleState) -> None:
        self._vs = vehicle_state

    @property
    def connected(self) -> bool:
        return bool(self._vs.snapshot.get("fc_connected", False))

    @property
    def port(self):
        return self._vs.snapshot.get("fc_port")

    @property
    def baud(self):
        return self._vs.snapshot.get("fc_baud")


__all__ = ["IpcVehicleState", "IpcParamCache", "IpcFcConnection"]
