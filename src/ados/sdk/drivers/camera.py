"""Camera driver base class.

A camera driver pumps frames from a physical or networked imaging
device into the agent's frame bus. Visible, thermal, depth, and
multi-spectral devices all share this interface.

The peripheral manager owns discovery and arbitration. A driver's job
is to answer :meth:`CameraDriver.discover` honestly, open sessions on
demand, and yield :class:`FrameBuffer` instances until the session
closes.

``CameraSession`` is whatever opaque state the driver needs (file
descriptors, decoder pipelines, vendor SDK handles). The host treats
it as a token and only passes it back to the driver's lifecycle and
streaming methods.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator


@dataclass(frozen=True)
class CameraCandidate:
    """A device a driver claims it can open.

    ``device_id`` should be stable across reboots when the underlying bus
    can produce a stable identifier (USB serial, fixed CSI lane, RTSP URL).
    Where stability is impossible (raw v4l2 ``/dev/videoN`` indexing) the
    driver should still produce a deterministic id within a single boot.
    """

    driver_id: str
    device_id: str
    label: str
    bus: str
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class CameraCapabilities:
    """Static capabilities a session reports after :meth:`CameraDriver.open`."""

    radiometric: bool
    bit_depth: int
    width: int
    height: int
    fps: float
    pixel_format: str
    streaming_protocol: str
    color_spaces: list[str]
    has_audio: bool = False


@dataclass(frozen=True)
class FrameBuffer:
    """One frame yielded by a camera driver.

    ``data`` is a ``memoryview`` so downstream consumers can avoid copying
    when feasible. ``radiometric_k`` carries the per-pixel temperature
    reconstruction matrix when the sensor is radiometric. Drivers may
    attach driver-specific fields under ``metadata``.
    """

    timestamp_ns: int
    sequence: int
    width: int
    height: int
    pixel_format: str
    data: memoryview
    radiometric_k: memoryview | None = None
    metadata: dict[str, Any] | None = None


class CameraSession:
    """Opaque per-open state held by a driver.

    Concrete drivers subclass this with whatever fields they need. The
    base class is intentionally empty so the host only treats sessions
    as tokens.
    """


class CameraDriver(ABC):
    """Abstract base for camera drivers.

    Implementations register themselves with ``peripheral_manager`` from
    a plugin's ``on_start`` hook. The host then calls :meth:`discover`,
    arbitrates among candidates, and routes selected devices through
    :meth:`open`.
    """

    @abstractmethod
    async def discover(self) -> list[CameraCandidate]:
        """Scan for devices this driver can open.

        Called on agent boot and on hotplug events. Returns the list of
        candidate devices the driver is willing to claim. An empty list
        means the driver found nothing it recognises and is fine.
        """

    @abstractmethod
    async def open(
        self, candidate: CameraCandidate, config: dict[str, Any]
    ) -> CameraSession:
        """Open a session against a device.

        Raises :class:`ados.sdk.drivers.errors.DriverError` (or a subclass)
        on failure. ``config`` carries driver-specific options validated
        against the plugin's ``config-schema.json``.
        """

    @abstractmethod
    async def close(self, session: CameraSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: CameraSession) -> CameraCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def frame_iterator(
        self, session: CameraSession
    ) -> AsyncIterator[FrameBuffer]:
        """Yield frames until the session closes or the iterator is cancelled."""

    @abstractmethod
    async def set_param(
        self, session: CameraSession, param: str, value: Any
    ) -> None:
        """Set a runtime parameter (gain, exposure, palette, shutter, etc.).

        Drivers should raise ``ValueError`` for unknown parameter names so
        the GCS panel can surface the rejection cleanly.
        """
