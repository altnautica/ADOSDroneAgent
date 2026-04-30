"""GPS driver base class.

A GPS driver decodes a position fix stream from a u-blox, NMEA, RTK,
or vendor-custom receiver and exposes it as a series of :class:`GpsFix`
samples. Drivers that support RTK can also accept RTCM correction
payloads via :meth:`GpsDriver.inject_rtcm`.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator


@dataclass(frozen=True)
class GpsCandidate:
    """A GPS receiver a driver claims it can open."""

    driver_id: str
    device_id: str
    label: str
    bus: str
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class GpsCapabilities:
    """Static capabilities of an open GPS session."""

    protocol: str
    constellations: list[str]
    max_update_hz: float
    supports_rtk: bool = False
    supports_dual_band: bool = False
    supports_heading: bool = False


@dataclass(frozen=True)
class GpsFix:
    """One position fix yielded by a GPS driver.

    Latitude and longitude are in degrees (WGS-84). Altitude is metres
    above mean sea level. ``fix_type`` follows the common convention:
    0 no fix, 2 2D, 3 3D, 4 DGPS, 5 RTK float, 6 RTK fixed.
    """

    timestamp_ns: int
    latitude_deg: float
    longitude_deg: float
    altitude_msl_m: float
    fix_type: int
    satellites_used: int
    hdop: float
    vdop: float
    horizontal_accuracy_m: float | None = None
    vertical_accuracy_m: float | None = None
    speed_mps: float | None = None
    course_deg: float | None = None
    heading_deg: float | None = None
    metadata: dict[str, Any] | None = None


class GpsSession:
    """Opaque per-open state held by a GPS driver."""


class GpsDriver(ABC):
    """Abstract base for GPS drivers."""

    @abstractmethod
    async def discover(self) -> list[GpsCandidate]:
        """Scan for GPS receivers this driver can open."""

    @abstractmethod
    async def open(
        self, candidate: GpsCandidate, config: dict[str, Any]
    ) -> GpsSession:
        """Open a session against a GPS receiver."""

    @abstractmethod
    async def close(self, session: GpsSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: GpsSession) -> GpsCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def fix_iterator(self, session: GpsSession) -> AsyncIterator[GpsFix]:
        """Yield fixes until the session closes."""

    @abstractmethod
    async def inject_rtcm(self, session: GpsSession, payload: bytes) -> None:
        """Forward an RTCM correction packet into the receiver.

        Receivers without RTK support should raise :class:`NotImplementedError`.
        """
