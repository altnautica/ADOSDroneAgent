"""ESC telemetry driver base class.

An ESC driver reads per-motor telemetry from electronic speed
controllers and surfaces it as a stream of :class:`EscTelemetry`
samples. DShot-telemetry, KISS, and BLHeli32 protocols all share this
interface. ESC drivers are read-only by design; setpoint commands flow
through the FC, not through the agent.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator


@dataclass(frozen=True)
class EscCandidate:
    """An ESC bank a driver claims it can open."""

    driver_id: str
    device_id: str
    label: str
    bus: str
    motor_count: int
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class EscCapabilities:
    """Static capabilities of an open ESC session."""

    protocol: str
    motor_count: int
    has_rpm: bool
    has_temperature: bool
    has_voltage: bool
    has_current: bool
    update_hz: float


@dataclass(frozen=True)
class EscTelemetry:
    """One telemetry sample for one motor.

    ``throttle_pct`` is the most recently commanded throttle from the FC,
    range 0 to 100. ``rpm`` is mechanical RPM (already corrected for pole
    count when the driver knows it).
    """

    timestamp_ns: int
    motor_index: int
    rpm: float
    temp_c: float
    voltage_v: float
    current_a: float
    throttle_pct: float
    metadata: dict[str, Any] | None = None


class EscSession:
    """Opaque per-open state held by an ESC driver."""


class EscDriver(ABC):
    """Abstract base for ESC telemetry drivers."""

    @abstractmethod
    async def discover(self) -> list[EscCandidate]:
        """Scan for ESC banks this driver can open."""

    @abstractmethod
    async def open(
        self, candidate: EscCandidate, config: dict[str, Any]
    ) -> EscSession:
        """Open a session against an ESC bank."""

    @abstractmethod
    async def close(self, session: EscSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: EscSession) -> EscCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def telemetry_iterator(
        self, session: EscSession
    ) -> AsyncIterator[EscTelemetry]:
        """Yield per-motor telemetry samples until the session closes."""
