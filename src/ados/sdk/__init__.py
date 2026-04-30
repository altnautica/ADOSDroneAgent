"""Plugin author SDK.

Re-exports the stable surface that hardware-driver and feature plugins
consume. Plugin code should import from this package, not from internal
agent modules.

Today this surface covers the driver-layer base classes for camera,
gimbal, LiDAR, GPS, ESC, and payload actuator hardware. More surface
(plugin context, event helpers, MAVLink component helpers) lands here
as the host APIs stabilise.
"""

from __future__ import annotations

from ados.sdk.testing import PluginTestHarness, load_fixture
from ados.sdk.drivers import (
    CameraCandidate,
    CameraCapabilities,
    CameraDriver,
    CameraSession,
    DriverDeviceNotFound,
    DriverError,
    DriverPermissionDenied,
    EscCandidate,
    EscCapabilities,
    EscDriver,
    EscSession,
    EscTelemetry,
    FrameBuffer,
    GimbalCandidate,
    GimbalCapabilities,
    GimbalDriver,
    GimbalSession,
    GimbalState,
    GpsCandidate,
    GpsCapabilities,
    GpsDriver,
    GpsFix,
    GpsSession,
    LidarCandidate,
    LidarCapabilities,
    LidarDriver,
    LidarFrame,
    LidarPoint,
    LidarSession,
    PayloadActuatorDriver,
    PayloadCandidate,
    PayloadCapabilities,
    PayloadCommand,
    PayloadSession,
    PayloadState,
)

__all__ = [
    # Testing
    "PluginTestHarness",
    "load_fixture",
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
