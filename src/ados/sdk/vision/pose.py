"""Pose + odometry encoders for visual-inertial pose injection toward the FC."""

from __future__ import annotations

from dataclasses import dataclass

from ados.sdk.vision._constants import (
    _DEFAULT_SYS_ID,
    POSE_COVARIANCE_LEN,
    VIO_COMPONENT_ID,
)
from ados.services.mavlink.encoders import (
    encode_odometry,
    encode_vision_position_estimate,
)

# ---------------------------------------------------------------------------
# Pose + odometry (visual-inertial pose injection toward the FC).
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Pose:
    """A pose estimate a vision plugin feeds to the flight controller.

    ``position`` is the local NED position in metres ``(x, y, z)``.
    ``orientation`` is the body-to-local rotation as a quaternion
    ``(w, x, y, z)`` (``(1, 0, 0, 0)`` is the null rotation). ``timestamp_us``
    is the capture time in microseconds. ``covariance``, when present, is the
    21-entry upper triangle of the 6x6 pose cross-covariance; ``None`` sends
    the MAVLink "unknown" marker (NaN in the first element).
    """

    position: tuple[float, float, float]
    orientation: tuple[float, float, float, float]
    timestamp_us: int
    covariance: tuple[float, ...] | None = None

    @classmethod
    def identity(cls, timestamp_us: int) -> Pose:
        """A pose with an identity orientation and no covariance, at origin."""
        return cls(
            position=(0.0, 0.0, 0.0),
            orientation=(1.0, 0.0, 0.0, 0.0),
            timestamp_us=int(timestamp_us),
            covariance=None,
        )

    def covariance_field(self) -> list[float]:
        """The covariance as MAVLink wants it: the supplied 21 entries, or the
        unknown marker (NaN in element 0) when absent."""
        if self.covariance is not None:
            c = list(self.covariance)
            if len(c) != POSE_COVARIANCE_LEN:
                raise ValueError(
                    f"covariance must have length {POSE_COVARIANCE_LEN}, "
                    f"got {len(c)}"
                )
            return c
        c = [0.0] * POSE_COVARIANCE_LEN
        c[0] = float("nan")
        return c

    def euler_rpy(self) -> tuple[float, float, float]:
        """Euler attitude ``(roll, pitch, yaw)`` in radians from the quaternion
        ``(w, x, y, z)``, for VISION_POSITION_ESTIMATE which carries Euler
        angles rather than a quaternion. Standard aerospace 3-2-1 (ZYX)
        sequence."""
        import math

        w, x, y, z = self.orientation
        # roll (x-axis rotation)
        sinr_cosp = 2.0 * (w * x + y * z)
        cosr_cosp = 1.0 - 2.0 * (x * x + y * y)
        roll = math.atan2(sinr_cosp, cosr_cosp)
        # pitch (y-axis rotation), clamped at the poles to avoid NaN from asin.
        sinp = 2.0 * (w * y - z * x)
        if abs(sinp) >= 1.0:
            pitch = math.copysign(math.pi / 2.0, sinp)
        else:
            pitch = math.asin(sinp)
        # yaw (z-axis rotation)
        siny_cosp = 2.0 * (w * z + x * y)
        cosy_cosp = 1.0 - 2.0 * (y * y + z * z)
        yaw = math.atan2(siny_cosp, cosy_cosp)
        return (roll, pitch, yaw)

    def to_vision_position_estimate_frame(
        self, *, sys_id: int = _DEFAULT_SYS_ID
    ) -> bytes:
        """Build the VISION_POSITION_ESTIMATE frame for this pose under the
        visual-odometry component id. The attitude is converted from the
        quaternion to Euler angles to match the message layout."""
        roll, pitch, yaw = self.euler_rpy()
        return encode_vision_position_estimate(
            sys_id=sys_id,
            comp_id=VIO_COMPONENT_ID,
            seq=0,
            usec=int(self.timestamp_us),
            x=self.position[0],
            y=self.position[1],
            z=self.position[2],
            roll=roll,
            pitch=pitch,
            yaw=yaw,
            covariance=self.covariance_field(),
        )


@dataclass(frozen=True)
class Odometry:
    """A full odometry estimate: pose plus body-frame linear and angular
    velocity.

    Wraps a :class:`Pose` and adds the twist a VISION_POSITION_ESTIMATE cannot
    carry. ``linear_velocity`` is ``(vx, vy, vz)`` in m/s and
    ``angular_velocity`` is ``(rollspeed, pitchspeed, yawspeed)`` in rad/s, both
    in the child (body) frame. ``velocity_covariance``, when present, is the
    21-entry upper triangle of the 6x6 velocity cross-covariance.
    """

    pose: Pose
    linear_velocity: tuple[float, float, float]
    angular_velocity: tuple[float, float, float]
    velocity_covariance: tuple[float, ...] | None = None

    def _velocity_covariance_field(self) -> list[float]:
        if self.velocity_covariance is not None:
            c = list(self.velocity_covariance)
            if len(c) != POSE_COVARIANCE_LEN:
                raise ValueError(
                    f"velocity_covariance must have length "
                    f"{POSE_COVARIANCE_LEN}, got {len(c)}"
                )
            return c
        c = [0.0] * POSE_COVARIANCE_LEN
        c[0] = float("nan")
        return c

    def to_odometry_frame(self, *, sys_id: int = _DEFAULT_SYS_ID) -> bytes:
        """Build the ODOMETRY frame. The reference frames are local NED for the
        pose and body NED for the twist, matching a forward-facing VIO source
        feeding the autopilot.
        """
        w, x, y, z = self.pose.orientation
        # MAV_FRAME_LOCAL_NED (1) for the pose, MAV_FRAME_BODY_NED (8) for the
        # twist child frame.
        return encode_odometry(
            sys_id=sys_id,
            comp_id=VIO_COMPONENT_ID,
            seq=0,
            time_usec=int(self.pose.timestamp_us),
            frame_id=1,
            child_frame_id=8,
            x=self.pose.position[0],
            y=self.pose.position[1],
            z=self.pose.position[2],
            q=[w, x, y, z],
            vx=self.linear_velocity[0],
            vy=self.linear_velocity[1],
            vz=self.linear_velocity[2],
            rollspeed=self.angular_velocity[0],
            pitchspeed=self.angular_velocity[1],
            yawspeed=self.angular_velocity[2],
            pose_covariance=self.pose.covariance_field(),
            velocity_covariance=self._velocity_covariance_field(),
        )
