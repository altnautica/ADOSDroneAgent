"""Driver-layer base class contract tests.

The base classes are pure interface. Concrete behaviour is exercised by
the drivers shipped in extension repositories. Here we just lock in the
shape: abstract instantiation refused, trivial subclasses accepted, and
value dataclasses frozen.
"""

from __future__ import annotations

from dataclasses import FrozenInstanceError
from typing import AsyncIterator

import pytest

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
from ados.plugins.errors import PluginError


# ---------------------------------------------------------------------------
# Base class abstract-ness
# ---------------------------------------------------------------------------

ABSTRACT_BASES = [
    CameraDriver,
    GimbalDriver,
    LidarDriver,
    GpsDriver,
    EscDriver,
    PayloadActuatorDriver,
]


@pytest.mark.parametrize("base", ABSTRACT_BASES)
def test_base_class_is_abstract(base: type) -> None:
    with pytest.raises(TypeError):
        base()  # type: ignore[abstract]


# ---------------------------------------------------------------------------
# Trivial concrete subclasses are instantiable
# ---------------------------------------------------------------------------


class _StubCamera(CameraDriver):
    async def discover(self) -> list[CameraCandidate]:
        return []

    async def open(
        self, candidate: CameraCandidate, config: dict
    ) -> CameraSession:
        return CameraSession()

    async def close(self, session: CameraSession) -> None:
        return None

    def capabilities(self, session: CameraSession) -> CameraCapabilities:
        return CameraCapabilities(
            radiometric=False,
            bit_depth=8,
            width=1,
            height=1,
            fps=1.0,
            pixel_format="Y8",
            streaming_protocol="v4l2",
            color_spaces=[],
        )

    async def frame_iterator(
        self, session: CameraSession
    ) -> AsyncIterator[FrameBuffer]:
        if False:
            yield  # pragma: no cover
        return

    async def set_param(
        self, session: CameraSession, param: str, value: object
    ) -> None:
        return None


class _StubGimbal(GimbalDriver):
    async def discover(self) -> list[GimbalCandidate]:
        return []

    async def open(
        self, candidate: GimbalCandidate, config: dict
    ) -> GimbalSession:
        return GimbalSession()

    async def close(self, session: GimbalSession) -> None:
        return None

    def capabilities(self, session: GimbalSession) -> GimbalCapabilities:
        return GimbalCapabilities(
            has_pitch=True,
            has_yaw=True,
            has_roll=False,
            pitch_min_deg=-90.0,
            pitch_max_deg=30.0,
            yaw_min_deg=-180.0,
            yaw_max_deg=180.0,
            roll_min_deg=0.0,
            roll_max_deg=0.0,
        )

    async def command_attitude(
        self,
        session: GimbalSession,
        pitch_deg: float,
        yaw_deg: float,
        roll_deg: float = 0.0,
    ) -> None:
        return None

    async def command_rate(
        self,
        session: GimbalSession,
        pitch_rate_dps: float,
        yaw_rate_dps: float,
        roll_rate_dps: float = 0.0,
    ) -> None:
        return None

    def get_state(self, session: GimbalSession) -> GimbalState:
        return GimbalState(
            timestamp_ns=0,
            pitch_deg=0.0,
            yaw_deg=0.0,
            roll_deg=0.0,
        )

    async def state_iterator(
        self, session: GimbalSession
    ) -> AsyncIterator[GimbalState]:
        if False:
            yield  # pragma: no cover
        return


class _StubLidar(LidarDriver):
    async def discover(self) -> list[LidarCandidate]:
        return []

    async def open(
        self, candidate: LidarCandidate, config: dict
    ) -> LidarSession:
        return LidarSession()

    async def close(self, session: LidarSession) -> None:
        return None

    def capabilities(self, session: LidarSession) -> LidarCapabilities:
        return LidarCapabilities(
            min_range_m=0.1,
            max_range_m=20.0,
            horizontal_fov_deg=360.0,
            vertical_fov_deg=1.0,
            points_per_frame=360,
            fps=10.0,
        )

    async def frame_iterator(
        self, session: LidarSession
    ) -> AsyncIterator[LidarFrame]:
        if False:
            yield  # pragma: no cover
        return

    async def set_param(
        self, session: LidarSession, param: str, value: object
    ) -> None:
        return None


class _StubGps(GpsDriver):
    async def discover(self) -> list[GpsCandidate]:
        return []

    async def open(self, candidate: GpsCandidate, config: dict) -> GpsSession:
        return GpsSession()

    async def close(self, session: GpsSession) -> None:
        return None

    def capabilities(self, session: GpsSession) -> GpsCapabilities:
        return GpsCapabilities(
            protocol="ubx",
            constellations=["GPS"],
            max_update_hz=10.0,
        )

    async def fix_iterator(self, session: GpsSession) -> AsyncIterator[GpsFix]:
        if False:
            yield  # pragma: no cover
        return

    async def inject_rtcm(self, session: GpsSession, payload: bytes) -> None:
        return None


class _StubEsc(EscDriver):
    async def discover(self) -> list[EscCandidate]:
        return []

    async def open(self, candidate: EscCandidate, config: dict) -> EscSession:
        return EscSession()

    async def close(self, session: EscSession) -> None:
        return None

    def capabilities(self, session: EscSession) -> EscCapabilities:
        return EscCapabilities(
            protocol="dshot-telemetry",
            motor_count=4,
            has_rpm=True,
            has_temperature=True,
            has_voltage=True,
            has_current=False,
            update_hz=50.0,
        )

    async def telemetry_iterator(
        self, session: EscSession
    ) -> AsyncIterator[EscTelemetry]:
        if False:
            yield  # pragma: no cover
        return


class _StubPayload(PayloadActuatorDriver):
    async def discover(self) -> list[PayloadCandidate]:
        return []

    async def open(
        self, candidate: PayloadCandidate, config: dict
    ) -> PayloadSession:
        return PayloadSession()

    async def close(self, session: PayloadSession) -> None:
        return None

    def capabilities(self, session: PayloadSession) -> PayloadCapabilities:
        return PayloadCapabilities(actions=("drop",))

    async def actuate(
        self, session: PayloadSession, command: PayloadCommand
    ) -> None:
        return None

    def get_state(self, session: PayloadSession) -> PayloadState:
        return PayloadState(
            timestamp_ns=0,
            last_action_id=None,
            busy=False,
        )


CONCRETE_STUBS = [
    _StubCamera,
    _StubGimbal,
    _StubLidar,
    _StubGps,
    _StubEsc,
    _StubPayload,
]


@pytest.mark.parametrize("stub", CONCRETE_STUBS)
def test_trivial_subclass_is_instantiable(stub: type) -> None:
    instance = stub()
    assert instance is not None


# ---------------------------------------------------------------------------
# Value dataclasses are frozen
# ---------------------------------------------------------------------------


def test_camera_candidate_frozen() -> None:
    c = CameraCandidate(
        driver_id="x", device_id="y", label="z", bus="usb-1.0"
    )
    with pytest.raises(FrozenInstanceError):
        c.label = "renamed"  # type: ignore[misc]


def test_frame_buffer_frozen() -> None:
    fb = FrameBuffer(
        timestamp_ns=0,
        sequence=0,
        width=1,
        height=1,
        pixel_format="Y8",
        data=memoryview(b"\x00"),
    )
    with pytest.raises(FrozenInstanceError):
        fb.sequence = 1  # type: ignore[misc]


def test_gimbal_state_frozen() -> None:
    s = GimbalState(
        timestamp_ns=0, pitch_deg=0.0, yaw_deg=0.0, roll_deg=0.0
    )
    with pytest.raises(FrozenInstanceError):
        s.pitch_deg = 1.0  # type: ignore[misc]


def test_lidar_frame_frozen() -> None:
    f = LidarFrame(timestamp_ns=0, sequence=0, points=())
    with pytest.raises(FrozenInstanceError):
        f.sequence = 1  # type: ignore[misc]


def test_lidar_point_frozen() -> None:
    p = LidarPoint(x=0.0, y=0.0, z=0.0)
    with pytest.raises(FrozenInstanceError):
        p.x = 1.0  # type: ignore[misc]


def test_gps_fix_frozen() -> None:
    f = GpsFix(
        timestamp_ns=0,
        latitude_deg=0.0,
        longitude_deg=0.0,
        altitude_msl_m=0.0,
        fix_type=3,
        satellites_used=8,
        hdop=1.0,
        vdop=1.0,
    )
    with pytest.raises(FrozenInstanceError):
        f.fix_type = 0  # type: ignore[misc]


def test_esc_telemetry_frozen() -> None:
    t = EscTelemetry(
        timestamp_ns=0,
        motor_index=0,
        rpm=1000.0,
        temp_c=30.0,
        voltage_v=16.0,
        current_a=5.0,
        throttle_pct=50.0,
    )
    with pytest.raises(FrozenInstanceError):
        t.rpm = 2000.0  # type: ignore[misc]


def test_payload_command_frozen() -> None:
    c = PayloadCommand(action_id="drop", args={})
    with pytest.raises(FrozenInstanceError):
        c.action_id = "release"  # type: ignore[misc]


# ---------------------------------------------------------------------------
# Error hierarchy
# ---------------------------------------------------------------------------


def test_driver_error_chains_under_plugin_error() -> None:
    assert issubclass(DriverError, PluginError)
    assert issubclass(DriverDeviceNotFound, DriverError)
    assert issubclass(DriverPermissionDenied, DriverError)
