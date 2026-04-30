"""Gimbal driver base class.

A gimbal driver moves a stabilised mount to a commanded attitude or
rate, and reports back the current pointing state. Vendor-specific
serial mounts, SBGC-family controllers, and MAVLink mount protocols
all share this interface.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator


@dataclass(frozen=True)
class GimbalCandidate:
    """A gimbal a driver claims it can open."""

    driver_id: str
    device_id: str
    label: str
    bus: str
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class GimbalCapabilities:
    """Static capabilities of an open gimbal session.

    Pitch, yaw, and roll limits are degrees from neutral. ``None`` on a
    rate limit means the axis is position-controlled only.
    """

    has_pitch: bool
    has_yaw: bool
    has_roll: bool
    pitch_min_deg: float
    pitch_max_deg: float
    yaw_min_deg: float
    yaw_max_deg: float
    roll_min_deg: float
    roll_max_deg: float
    max_rate_dps: float | None = None
    supports_follow_mode: bool = False
    supports_lock_mode: bool = False


@dataclass(frozen=True)
class GimbalState:
    """A single state sample reported by a gimbal."""

    timestamp_ns: int
    pitch_deg: float
    yaw_deg: float
    roll_deg: float
    pitch_rate_dps: float = 0.0
    yaw_rate_dps: float = 0.0
    roll_rate_dps: float = 0.0
    mode: str = "neutral"
    metadata: dict[str, Any] | None = None


class GimbalSession:
    """Opaque per-open state held by a gimbal driver."""


class GimbalDriver(ABC):
    """Abstract base for gimbal drivers."""

    @abstractmethod
    async def discover(self) -> list[GimbalCandidate]:
        """Scan for gimbals this driver can open."""

    @abstractmethod
    async def open(
        self, candidate: GimbalCandidate, config: dict[str, Any]
    ) -> GimbalSession:
        """Open a session against a gimbal."""

    @abstractmethod
    async def close(self, session: GimbalSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: GimbalSession) -> GimbalCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def command_attitude(
        self,
        session: GimbalSession,
        pitch_deg: float,
        yaw_deg: float,
        roll_deg: float = 0.0,
    ) -> None:
        """Drive the gimbal to an absolute pitch, yaw, and roll setpoint.

        Out-of-range setpoints should be clamped against
        :class:`GimbalCapabilities` rather than silently ignored.
        """

    @abstractmethod
    async def command_rate(
        self,
        session: GimbalSession,
        pitch_rate_dps: float,
        yaw_rate_dps: float,
        roll_rate_dps: float = 0.0,
    ) -> None:
        """Drive the gimbal at a commanded angular rate.

        Drivers without rate control should raise :class:`NotImplementedError`
        so the host can fall back to position commands.
        """

    @abstractmethod
    def get_state(self, session: GimbalSession) -> GimbalState:
        """Return the most recent attitude sample."""

    @abstractmethod
    async def state_iterator(
        self, session: GimbalSession
    ) -> AsyncIterator[GimbalState]:
        """Yield attitude samples until the session closes."""
