"""Driver-layer base classes for hardware plugins.

Hardware-driver plugins subclass one of these base classes and register
the resulting driver instance with the agent's peripheral manager from
their ``on_start`` hook. The peripheral manager handles device
discovery, arbitration among competing drivers, session lifecycle, and
event-bus publication of frames, fixes, telemetry, and state.

Public re-exports below are the stable surface plugin authors consume.
"""

from __future__ import annotations

from ados.sdk.drivers.camera import (
    CameraCandidate,
    CameraCapabilities,
    CameraDriver,
    CameraSession,
    FrameBuffer,
)
from ados.sdk.drivers.errors import (
    DriverDeviceNotFound,
    DriverError,
    DriverPermissionDenied,
)
from ados.sdk.drivers.esc import (
    EscCandidate,
    EscCapabilities,
    EscDriver,
    EscSession,
    EscTelemetry,
)
from ados.sdk.drivers.gimbal import (
    GimbalCandidate,
    GimbalCapabilities,
    GimbalDriver,
    GimbalSession,
    GimbalState,
)
from ados.sdk.drivers.gps import (
    GpsCandidate,
    GpsCapabilities,
    GpsDriver,
    GpsFix,
    GpsSession,
)
from ados.sdk.drivers.lidar import (
    LidarCandidate,
    LidarCapabilities,
    LidarDriver,
    LidarFrame,
    LidarPoint,
    LidarSession,
)
from ados.sdk.drivers.payload_actuator import (
    PayloadActuatorDriver,
    PayloadCandidate,
    PayloadCapabilities,
    PayloadCommand,
    PayloadSession,
    PayloadState,
)

__all__ = [
    # Errors
    "DriverError",
    "DriverDeviceNotFound",
    "DriverPermissionDenied",
    # Camera
    "CameraDriver",
    "CameraSession",
    "CameraCandidate",
    "CameraCapabilities",
    "FrameBuffer",
    # Gimbal
    "GimbalDriver",
    "GimbalSession",
    "GimbalCandidate",
    "GimbalCapabilities",
    "GimbalState",
    # LiDAR
    "LidarDriver",
    "LidarSession",
    "LidarCandidate",
    "LidarCapabilities",
    "LidarFrame",
    "LidarPoint",
    # GPS
    "GpsDriver",
    "GpsSession",
    "GpsCandidate",
    "GpsCapabilities",
    "GpsFix",
    # ESC
    "EscDriver",
    "EscSession",
    "EscCandidate",
    "EscCapabilities",
    "EscTelemetry",
    # Payload actuator
    "PayloadActuatorDriver",
    "PayloadSession",
    "PayloadCandidate",
    "PayloadCapabilities",
    "PayloadCommand",
    "PayloadState",
]
