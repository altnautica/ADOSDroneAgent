"""System health monitoring for ADOS Drone Agent."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import psutil

from ados.core.logging import get_logger

log = get_logger("health")


# Thermal zone types worth surfacing as the SoC temperature. The order
# is preference: a "soc-thermal" reading beats a "littlecore-thermal"
# reading on Rockchip platforms which expose both. psutil already covers
# x86 (coretemp) and Pi (cpu_thermal); the sysfs fallback below catches
# Rockchip, Allwinner, and other ARM SoCs whose kernels expose readings
# only through /sys/class/thermal.
_THERMAL_ZONE_KEYS: tuple[str, ...] = (
    "soc-thermal",
    "soc_thermal",
    "cpu-thermal",
    "cpu_thermal",
    "littlecore-thermal",
    "bigcore0-thermal",
    "bigcore1-thermal",
    "gpu-thermal",
)


def _read_temperature() -> float | None:
    """Best-effort SoC temperature in °C across psutil + sysfs.

    Falls through three tiers:
      1. ``psutil.sensors_temperatures()`` — works on x86 and Pi.
      2. ``/sys/class/thermal/thermal_zone*`` — works on Rockchip,
         Allwinner, and most ARM SoCs once a thermal driver loads.
      3. ``None`` — the kernel exposes nothing useful.
    """
    try:
        temps = psutil.sensors_temperatures()
        if temps:
            for key in ("cpu_thermal", "cpu-thermal", "coretemp"):
                if key in temps and temps[key]:
                    return float(temps[key][0].current)
    except (AttributeError, OSError):
        pass

    try:
        zones = sorted(Path("/sys/class/thermal").glob("thermal_zone*"))
    except OSError:
        return None

    best_priority = len(_THERMAL_ZONE_KEYS)
    best_celsius: float | None = None
    fallback: float | None = None
    for zone in zones:
        try:
            zone_type = (zone / "type").read_text().strip().lower()
            millideg = int((zone / "temp").read_text().strip())
        except (OSError, ValueError):
            continue
        celsius = millideg / 1000.0
        if zone_type in _THERMAL_ZONE_KEYS:
            priority = _THERMAL_ZONE_KEYS.index(zone_type)
            if priority < best_priority:
                best_priority = priority
                best_celsius = celsius
        elif fallback is None:
            fallback = celsius
    return best_celsius if best_celsius is not None else fallback


@dataclass
class SystemHealth:
    cpu_percent: float = 0.0
    memory_percent: float = 0.0
    disk_percent: float = 0.0
    temperature: float | None = None
    timestamp: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_dict(self) -> dict:
        return {
            "cpu_percent": self.cpu_percent,
            "memory_percent": self.memory_percent,
            "disk_percent": self.disk_percent,
            "temperature": self.temperature,
            "timestamp": self.timestamp,
        }


class HealthMonitor:
    """Monitors system resources (CPU, RAM, disk, temperature)."""

    def __init__(self) -> None:
        self._last: SystemHealth = SystemHealth()

    def check_system(self) -> SystemHealth:
        """Check current system health."""
        temp = _read_temperature()

        self._last = SystemHealth(
            cpu_percent=psutil.cpu_percent(interval=0),
            memory_percent=psutil.virtual_memory().percent,
            disk_percent=psutil.disk_usage("/").percent,
            temperature=temp,
        )
        return self._last

    @property
    def last(self) -> SystemHealth:
        return self._last

    def sd_notify_ready(self) -> None:
        """Notify systemd that the service is ready."""
        try:
            import socket
            addr = "/run/systemd/notify"
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            sock.sendto(b"READY=1", addr)
            sock.close()
        except (OSError, FileNotFoundError):
            log.debug("sd_notify not available (not running under systemd)")

    def sd_notify_watchdog(self) -> None:
        """Send watchdog ping to systemd."""
        try:
            import socket
            addr = "/run/systemd/notify"
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            sock.sendto(b"WATCHDOG=1", addr)
            sock.close()
        except (OSError, FileNotFoundError):
            pass
