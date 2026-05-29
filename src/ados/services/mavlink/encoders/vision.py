"""Vision-source encoders for the agent.

Six messages cover the typical CV / VIO injection surface:

* OPTICAL_FLOW (100)              — flow-sensor scalar output
* OPTICAL_FLOW_RAD (106)          — angular-rate flow (PX4FLOW class)
* VISION_POSITION_ESTIMATE (102)  — local-frame pose from CV
* GLOBAL_VISION_POSITION_ESTIMATE (101) — global-frame pose from CV
* ODOMETRY (331)                  — full odometry (pose + twist + covariance)
* VISION_POSITION_DELTA (11011)   — body-frame delta (ardupilotmega dialect)

All functions take a sequence number from the caller. SEQ is a per
(sys_id, comp_id) monotonic counter outside the encoder's concern.

Wire field order is taken directly from each message's ordered field
list as published by the MAVLink generator. The order is NOT the same
as the declaration order in the XML for several of these messages — for
example OPTICAL_FLOW declares the integer flow_x / flow_y before the
float flow_comp_m_*, but the wire packs the floats first (largest type
size first). The struct format strings below reflect the wire order.
"""

from __future__ import annotations

import struct
from collections.abc import Sequence
from typing import Final

from ._framing import pack_v2

# Message ids
MSG_ID_OPTICAL_FLOW: Final = 100
MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE: Final = 101
MSG_ID_VISION_POSITION_ESTIMATE: Final = 102
MSG_ID_OPTICAL_FLOW_RAD: Final = 106
MSG_ID_ODOMETRY: Final = 331
MSG_ID_VISION_POSITION_DELTA: Final = 11011

# CRC_EXTRA values, dialect-fingerprinted. Confirmed against pymavlink's
# generated ardupilotmega dialect tables; the upstream MAVLink generator
# computes these from the field-type + field-name digest, so they're
# tied to wire-format identity and must match exactly for a frame to
# pass the receiver's checksum check.
CRC_OPTICAL_FLOW: Final = 175
CRC_GLOBAL_VISION_POSITION_ESTIMATE: Final = 102
CRC_VISION_POSITION_ESTIMATE: Final = 158
CRC_OPTICAL_FLOW_RAD: Final = 138
CRC_ODOMETRY: Final = 91
CRC_VISION_POSITION_DELTA: Final = 106

# Pre-built struct packers (wire order, little-endian).
_PACK_OPTICAL_FLOW = struct.Struct("<QfffhhBBff").pack
_PACK_GLOBAL_VPE = struct.Struct("<Qffffff21fB").pack
_PACK_VPE = struct.Struct("<Qffffff21fB").pack
_PACK_OPTICAL_FLOW_RAD = struct.Struct("<QIfffffIfhBB").pack
_PACK_ODOMETRY = struct.Struct("<Qfff4fffffff21f21fBBBBb").pack
_PACK_VPD = struct.Struct("<QQ3f3ff").pack


def _check_len(name: str, seq: Sequence[float], expected: int) -> None:
    if len(seq) != expected:
        raise ValueError(f"{name} must have length {expected}, got {len(seq)}")


def encode_optical_flow(
    sys_id: int,
    comp_id: int,
    seq: int,
    time_usec: int,
    sensor_id: int,
    flow_x: int,
    flow_y: int,
    flow_comp_m_x: float,
    flow_comp_m_y: float,
    quality: int,
    ground_distance: float,
    flow_rate_x: float = 0.0,
    flow_rate_y: float = 0.0,
) -> bytes:
    """Build an OPTICAL_FLOW (100) v2 frame."""
    payload = _PACK_OPTICAL_FLOW(
        time_usec, flow_comp_m_x, flow_comp_m_y, ground_distance,
        flow_x, flow_y, sensor_id, quality, flow_rate_x, flow_rate_y,
    )
    return pack_v2(
        msg_id=MSG_ID_OPTICAL_FLOW, crc_extra=CRC_OPTICAL_FLOW,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_optical_flow_rad(
    sys_id: int,
    comp_id: int,
    seq: int,
    time_usec: int,
    sensor_id: int,
    integration_time_us: int,
    integrated_x: float,
    integrated_y: float,
    integrated_xgyro: float,
    integrated_ygyro: float,
    integrated_zgyro: float,
    temperature: int,
    quality: int,
    time_delta_distance_us: int,
    distance: float,
) -> bytes:
    """Build an OPTICAL_FLOW_RAD (106) v2 frame (PX4FLOW-style sensor)."""
    payload = _PACK_OPTICAL_FLOW_RAD(
        time_usec, integration_time_us,
        integrated_x, integrated_y,
        integrated_xgyro, integrated_ygyro, integrated_zgyro,
        time_delta_distance_us, distance,
        temperature, sensor_id, quality,
    )
    return pack_v2(
        msg_id=MSG_ID_OPTICAL_FLOW_RAD, crc_extra=CRC_OPTICAL_FLOW_RAD,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_vision_position_estimate(
    sys_id: int,
    comp_id: int,
    seq: int,
    usec: int,
    x: float,
    y: float,
    z: float,
    roll: float,
    pitch: float,
    yaw: float,
    covariance: Sequence[float],
    reset_counter: int = 0,
) -> bytes:
    """Build a VISION_POSITION_ESTIMATE (102) v2 frame.

    `covariance` is the length-21 upper-triangular row-major matrix of
    the 6x6 pose covariance, as documented in common.xml.
    """
    _check_len("covariance", covariance, 21)
    payload = _PACK_VPE(usec, x, y, z, roll, pitch, yaw, *covariance, reset_counter)
    return pack_v2(
        msg_id=MSG_ID_VISION_POSITION_ESTIMATE, crc_extra=CRC_VISION_POSITION_ESTIMATE,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_global_vision_position_estimate(
    sys_id: int,
    comp_id: int,
    seq: int,
    usec: int,
    x: float,
    y: float,
    z: float,
    roll: float,
    pitch: float,
    yaw: float,
    covariance: Sequence[float],
    reset_counter: int = 0,
) -> bytes:
    """Build a GLOBAL_VISION_POSITION_ESTIMATE (101) v2 frame."""
    _check_len("covariance", covariance, 21)
    payload = _PACK_GLOBAL_VPE(usec, x, y, z, roll, pitch, yaw, *covariance, reset_counter)
    return pack_v2(
        msg_id=MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE,
        crc_extra=CRC_GLOBAL_VISION_POSITION_ESTIMATE,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_odometry(
    sys_id: int,
    comp_id: int,
    seq: int,
    time_usec: int,
    frame_id: int,
    child_frame_id: int,
    x: float,
    y: float,
    z: float,
    q: Sequence[float],
    vx: float,
    vy: float,
    vz: float,
    rollspeed: float,
    pitchspeed: float,
    yawspeed: float,
    pose_covariance: Sequence[float],
    velocity_covariance: Sequence[float],
    reset_counter: int = 0,
    estimator_type: int = 0,
    quality: int = 0,
) -> bytes:
    """Build an ODOMETRY (331) v2 frame.

    `q` is the attitude quaternion in [w, x, y, z] order, length 4.
    `pose_covariance` and `velocity_covariance` are length-21 upper-
    triangular row-major matrices, per the ODOMETRY spec. Note that
    ArduPilot's external-nav consumer reads only the diagonal entries
    at indices 0, 6, 11, 15, 18, 20 of `pose_covariance`; populating
    the off-diagonals is fine but ignored on that consumer.
    """
    _check_len("q", q, 4)
    _check_len("pose_covariance", pose_covariance, 21)
    _check_len("velocity_covariance", velocity_covariance, 21)
    payload = _PACK_ODOMETRY(
        time_usec, x, y, z, *q, vx, vy, vz, rollspeed, pitchspeed, yawspeed,
        *pose_covariance, *velocity_covariance,
        frame_id, child_frame_id, reset_counter, estimator_type, quality,
    )
    return pack_v2(
        msg_id=MSG_ID_ODOMETRY, crc_extra=CRC_ODOMETRY,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_vision_position_delta(
    sys_id: int,
    comp_id: int,
    seq: int,
    time_usec: int,
    time_delta_usec: int,
    angle_delta: Sequence[float],
    position_delta: Sequence[float],
    confidence: float,
) -> bytes:
    """Build a VISION_POSITION_DELTA (11011) v2 frame.

    ArduPilot-only dialect message. `angle_delta` is a length-3 rotation
    vector [roll, pitch, yaw] in body-FRD, and `position_delta` is the
    length-3 translation in the same frame.
    """
    _check_len("angle_delta", angle_delta, 3)
    _check_len("position_delta", position_delta, 3)
    payload = _PACK_VPD(time_usec, time_delta_usec, *angle_delta, *position_delta, confidence)
    return pack_v2(
        msg_id=MSG_ID_VISION_POSITION_DELTA, crc_extra=CRC_VISION_POSITION_DELTA,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )
