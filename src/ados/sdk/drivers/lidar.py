"""LiDAR driver base class.

A LiDAR driver yields point-cloud frames from a spinning, solid-state,
or single-line ranging device. RPLidar, Livox, Velodyne, and custom
UART or I2C rangefinders all share this interface.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator


@dataclass(frozen=True)
class LidarCandidate:
    """A LiDAR a driver claims it can open."""

    driver_id: str
    device_id: str
    label: str
    bus: str
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class LidarCapabilities:
    """Static capabilities of an open LiDAR session.

    ``points_per_frame`` is the typical count for a single rotation or
    sweep; the actual frame may carry fewer points if the sensor reports
    misses for some angles.
    """

    min_range_m: float
    max_range_m: float
    horizontal_fov_deg: float
    vertical_fov_deg: float
    points_per_frame: int
    fps: float
    has_intensity: bool = False
    has_dual_return: bool = False


@dataclass(frozen=True)
class LidarPoint:
    """A single point in a LiDAR frame.

    Coordinates are in metres in the sensor body frame: ``+x`` forward,
    ``+y`` left, ``+z`` up. Intensity is sensor-native and may be ``None``
    on devices that do not report it.
    """

    x: float
    y: float
    z: float
    intensity: float | None = None
    return_index: int = 0


@dataclass(frozen=True)
class LidarFrame:
    """One sweep or rotation of points yielded by a LiDAR driver."""

    timestamp_ns: int
    sequence: int
    points: tuple[LidarPoint, ...]
    metadata: dict[str, Any] | None = None


class LidarSession:
    """Opaque per-open state held by a LiDAR driver."""


class LidarDriver(ABC):
    """Abstract base for LiDAR drivers."""

    @abstractmethod
    async def discover(self) -> list[LidarCandidate]:
        """Scan for LiDARs this driver can open."""

    @abstractmethod
    async def open(
        self, candidate: LidarCandidate, config: dict[str, Any]
    ) -> LidarSession:
        """Open a session against a LiDAR."""

    @abstractmethod
    async def close(self, session: LidarSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: LidarSession) -> LidarCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def frame_iterator(
        self, session: LidarSession
    ) -> AsyncIterator[LidarFrame]:
        """Yield point-cloud frames until the session closes."""

    @abstractmethod
    async def set_param(
        self, session: LidarSession, param: str, value: Any
    ) -> None:
        """Set a runtime parameter (rpm, scan rate, filter mode)."""
