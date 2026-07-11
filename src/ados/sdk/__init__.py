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
from ados.sdk.offload import (
    FreshnessGate,
    GateState,
    LockGate,
    LockState,
)
from ados.sdk.testing import (
    FakeVisionEngine,
    PluginTestHarness,
    load_fixture,
)
from ados.sdk.tracking import (
    DEFAULT_COAST_WINDOW_S,
    LOCK_LOCKED,
    LOCK_LOST,
    LOCK_UNCERTAIN,
    EffectiveLock,
    LockedTarget,
    LockedTargetTracker,
)
from ados.sdk.vision import (
    BoundingBox,
    Detection,
    DetectionBatch,
    Frame,
    FrameDescriptor,
    FrameFormat,
    ModelExecution,
    ModelKind,
    ModelMetadata,
    Odometry,
    Pose,
    RingLayout,
    VisionClient,
)

__all__ = [
    # Testing
    "PluginTestHarness",
    "FakeVisionEngine",
    "load_fixture",
    # Locked-target behaviour gate (shared by target-driven plugins)
    "LockedTargetTracker",
    "LockedTarget",
    "EffectiveLock",
    "LOCK_LOCKED",
    "LOCK_UNCERTAIN",
    "LOCK_LOST",
    "DEFAULT_COAST_WINDOW_S",
    # Perception-offload freshness/link gate
    "FreshnessGate",
    "GateState",
    "LockGate",
    "LockState",
    # Vision
    "VisionClient",
    "FrameFormat",
    "FrameDescriptor",
    "Frame",
    "RingLayout",
    "ModelKind",
    "ModelExecution",
    "ModelMetadata",
    "BoundingBox",
    "Detection",
    "DetectionBatch",
    "Pose",
    "Odometry",
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
