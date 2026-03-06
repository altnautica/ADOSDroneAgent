"""System health monitoring for ADOS Drone Agent."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone

import psutil

from ados.core.logging import get_logger

log = get_logger("health")


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
        temp = None
        try:
            temps = psutil.sensors_temperatures()
            if temps:
                # Try common keys: cpu_thermal (RPi), coretemp (x86)
                for key in ("cpu_thermal", "cpu-thermal", "coretemp"):
                    if key in temps and temps[key]:
                        temp = temps[key][0].current
                        break
        except (AttributeError, OSError):
            pass

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
