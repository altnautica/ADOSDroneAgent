"""Rangefinder encoders.

Exposes DISTANCE_SENSOR (132) for any plugin that wants to publish a
range observation back into the flight stack. Useful for terrain-
following with an external LiDAR, sonar, or radar, and for obstacle-
avoidance fusion.
"""

from __future__ import annotations

import struct
from typing import Final, Sequence

from ._framing import pack_v2

MSG_ID_DISTANCE_SENSOR: Final = 132
CRC_DISTANCE_SENSOR: Final = 85

# Wire order: time_boot_ms, min_distance, max_distance, current_distance,
# type, id, orientation, covariance, horizontal_fov, vertical_fov,
# quaternion[4], signal_quality.
_PACK_DISTANCE_SENSOR = struct.Struct("<IHHHBBBBff4fB").pack


def encode_distance_sensor(
    sys_id: int,
    comp_id: int,
    seq: int,
    time_boot_ms: int,
    min_distance: int,
    max_distance: int,
    current_distance: int,
    type_: int,
    id_: int,
    orientation: int,
    covariance: int,
    horizontal_fov: float = 0.0,
    vertical_fov: float = 0.0,
    quaternion: Sequence[float] = (0.0, 0.0, 0.0, 0.0),
    signal_quality: int = 0,
) -> bytes:
    """Build a DISTANCE_SENSOR (132) v2 frame.

    `type_` and `id_` use the trailing-underscore convention because
    `type` and `id` are reserved names in Python. Distances are uint16
    in centimetres. `quaternion` is the sensor-attachment orientation
    if `orientation == MAV_SENSOR_ROTATION_CUSTOM`; otherwise the four
    floats may be left at zero. `signal_quality` is a 0-100 percent
    value, where 0 means "unknown" per the spec.
    """
    if len(quaternion) != 4:
        raise ValueError(f"quaternion must have length 4, got {len(quaternion)}")
    payload = _PACK_DISTANCE_SENSOR(
        time_boot_ms,
        min_distance, max_distance, current_distance,
        type_, id_, orientation, covariance,
        horizontal_fov, vertical_fov,
        quaternion[0], quaternion[1], quaternion[2], quaternion[3],
        signal_quality,
    )
    return pack_v2(
        msg_id=MSG_ID_DISTANCE_SENSOR, crc_extra=CRC_DISTANCE_SENSOR,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )
