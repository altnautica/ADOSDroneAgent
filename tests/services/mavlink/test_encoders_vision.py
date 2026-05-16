"""Round-trip tests for vision encoders.

Each encoder is exercised with non-trivial field values so empty-byte
truncation, signed-int handling, and quaternion / covariance unpacking
all get a genuine workout. The reference decoder is pymavlink, which is
already a runtime dependency of the agent.
"""

from __future__ import annotations

import math

import pytest

from ados.services.mavlink.encoders import (
    CRC_EXTRA_TABLE,
    ENCODER_CAPABILITY_GATES,
    MESSAGE_ID_TO_ENCODER,
    MESSAGE_NAMES,
    encode_global_vision_position_estimate,
    encode_odometry,
    encode_optical_flow,
    encode_optical_flow_rad,
    encode_vision_position_delta,
    encode_vision_position_estimate,
)


def _ramp(n: int) -> list[float]:
    """A length-n list of monotonically increasing floats, useful for
    covariance matrices where we want every byte position to vary."""
    return [0.5 + 0.25 * i for i in range(n)]


# ───────────────────────── OPTICAL_FLOW (100) ─────────────────────────


def test_optical_flow_round_trip(decoder):
    frame = encode_optical_flow(
        sys_id=42,
        comp_id=198,
        seq=7,
        time_usec=0xDEADBEEF,
        sensor_id=3,
        flow_x=-1234,
        flow_y=5678,
        flow_comp_m_x=0.125,
        flow_comp_m_y=-0.375,
        quality=200,
        ground_distance=1.75,
        flow_rate_x=0.1,
        flow_rate_y=-0.2,
    )
    assert frame[0] == 0xFD
    msg = decoder(frame)
    assert msg.get_type() == "OPTICAL_FLOW"
    assert msg.time_usec == 0xDEADBEEF
    assert msg.sensor_id == 3
    assert msg.flow_x == -1234
    assert msg.flow_y == 5678
    assert msg.quality == 200
    assert math.isclose(msg.flow_comp_m_x, 0.125)
    assert math.isclose(msg.flow_comp_m_y, -0.375)
    assert math.isclose(msg.ground_distance, 1.75)
    assert math.isclose(msg.flow_rate_x, 0.1, rel_tol=1e-6)
    assert math.isclose(msg.flow_rate_y, -0.2, rel_tol=1e-6)


def test_optical_flow_header_fields(decoder):
    frame = encode_optical_flow(
        sys_id=11, comp_id=22, seq=99,
        time_usec=1, sensor_id=0, flow_x=0, flow_y=0,
        flow_comp_m_x=1.0, flow_comp_m_y=1.0, quality=50,
        ground_distance=2.0,
    )
    # STX, len, incompat, compat, seq, sysid, compid, msgid (3 bytes LE)
    assert frame[0] == 0xFD
    assert frame[2] == 0  # incompat: unsigned
    assert frame[3] == 0  # compat
    assert frame[4] == 99
    assert frame[5] == 11
    assert frame[6] == 22
    assert frame[7] == 100  # msgid low
    assert frame[8] == 0
    assert frame[9] == 0
    msg = decoder(frame)
    assert msg.get_srcSystem() == 11
    assert msg.get_srcComponent() == 22
    assert msg.get_seq() == 99


# ─────────────────────── OPTICAL_FLOW_RAD (106) ───────────────────────


def test_optical_flow_rad_round_trip(decoder):
    frame = encode_optical_flow_rad(
        sys_id=1, comp_id=198, seq=0,
        time_usec=12_345,
        sensor_id=1,
        integration_time_us=10_000,
        integrated_x=0.01,
        integrated_y=-0.02,
        integrated_xgyro=0.001,
        integrated_ygyro=-0.001,
        integrated_zgyro=0.002,
        temperature=2500,
        quality=200,
        time_delta_distance_us=10_000,
        distance=1.25,
    )
    msg = decoder(frame)
    assert msg.get_type() == "OPTICAL_FLOW_RAD"
    assert msg.integration_time_us == 10_000
    assert msg.sensor_id == 1
    assert msg.temperature == 2500
    assert math.isclose(msg.distance, 1.25)
    assert math.isclose(msg.integrated_x, 0.01, rel_tol=1e-6)


def test_optical_flow_rad_empty_payload_truncation(decoder):
    """All-zero trailing fields should still decode cleanly thanks to v2
    empty-byte truncation. The frame must shrink below the full payload
    length but the decoder rehydrates the missing tail as zeros."""
    frame = encode_optical_flow_rad(
        sys_id=1, comp_id=198, seq=0,
        time_usec=0, sensor_id=0, integration_time_us=0,
        integrated_x=0.0, integrated_y=0.0,
        integrated_xgyro=0.0, integrated_ygyro=0.0, integrated_zgyro=0.0,
        temperature=0, quality=0,
        time_delta_distance_us=0, distance=0.0,
    )
    msg = decoder(frame)
    assert msg.distance == 0.0
    # Frame length: STX + 9 header + payload(≥1) + CRC(2) = ≥ 13.
    # Without truncation the payload would be 44 bytes for a total of 56.
    # With everything zero, truncation should knock that down to ~14.
    assert len(frame) < 56


# ──────────────────── VISION_POSITION_ESTIMATE (102) ──────────────────


def test_vision_position_estimate_round_trip(decoder):
    cov = _ramp(21)
    frame = encode_vision_position_estimate(
        sys_id=1, comp_id=196, seq=4,
        usec=1_000_000,
        x=1.0, y=2.0, z=-3.5,
        roll=0.1, pitch=-0.2, yaw=0.3,
        covariance=cov,
        reset_counter=7,
    )
    msg = decoder(frame)
    assert msg.get_type() == "VISION_POSITION_ESTIMATE"
    assert msg.usec == 1_000_000
    assert math.isclose(msg.x, 1.0)
    assert math.isclose(msg.z, -3.5)
    assert msg.reset_counter == 7
    for got, want in zip(msg.covariance, cov):
        assert math.isclose(got, want, rel_tol=1e-6)


def test_vision_position_estimate_rejects_bad_covariance():
    with pytest.raises(ValueError, match="covariance"):
        encode_vision_position_estimate(
            sys_id=1, comp_id=196, seq=0,
            usec=0, x=0, y=0, z=0, roll=0, pitch=0, yaw=0,
            covariance=[0.0] * 20,  # too short
        )


# ─────────────── GLOBAL_VISION_POSITION_ESTIMATE (101) ────────────────


def test_global_vision_position_estimate_round_trip(decoder):
    cov = _ramp(21)
    frame = encode_global_vision_position_estimate(
        sys_id=1, comp_id=196, seq=5,
        usec=2_000_000,
        x=10.0, y=-20.0, z=5.5,
        roll=-0.1, pitch=0.2, yaw=-0.3,
        covariance=cov, reset_counter=2,
    )
    msg = decoder(frame)
    assert msg.get_type() == "GLOBAL_VISION_POSITION_ESTIMATE"
    assert msg.usec == 2_000_000
    assert math.isclose(msg.x, 10.0)
    assert math.isclose(msg.y, -20.0)
    assert math.isclose(msg.z, 5.5)
    assert msg.reset_counter == 2


# ─────────────────────────── ODOMETRY (331) ───────────────────────────


def test_odometry_round_trip(decoder):
    q = [1.0, 0.0, 0.0, 0.0]  # identity quaternion
    pose_cov = _ramp(21)
    vel_cov = [0.01 * i for i in range(21)]
    frame = encode_odometry(
        sys_id=1, comp_id=197, seq=11,
        time_usec=3_000_000,
        frame_id=1, child_frame_id=8,
        x=0.5, y=-0.25, z=-1.0,
        q=q,
        vx=0.1, vy=0.0, vz=0.0,
        rollspeed=0.0, pitchspeed=0.0, yawspeed=0.0,
        pose_covariance=pose_cov,
        velocity_covariance=vel_cov,
        reset_counter=3,
        estimator_type=2,
        quality=88,
    )
    msg = decoder(frame)
    assert msg.get_type() == "ODOMETRY"
    assert msg.frame_id == 1
    assert msg.child_frame_id == 8
    assert msg.reset_counter == 3
    assert msg.estimator_type == 2
    assert msg.quality == 88
    for got, want in zip(msg.q, q):
        assert math.isclose(got, want)
    # Spot-check the diagonal entries ArduPilot actually reads.
    for idx in (0, 6, 11, 15, 18, 20):
        assert math.isclose(msg.pose_covariance[idx], pose_cov[idx], rel_tol=1e-6)


def test_odometry_rejects_bad_q():
    with pytest.raises(ValueError, match="q"):
        encode_odometry(
            sys_id=1, comp_id=197, seq=0,
            time_usec=0, frame_id=0, child_frame_id=0,
            x=0, y=0, z=0,
            q=[1.0, 0.0, 0.0],  # only 3
            vx=0, vy=0, vz=0,
            rollspeed=0, pitchspeed=0, yawspeed=0,
            pose_covariance=[0.0] * 21,
            velocity_covariance=[0.0] * 21,
        )


# ─────────────────── VISION_POSITION_DELTA (11011) ────────────────────


def test_vision_position_delta_round_trip(decoder):
    frame = encode_vision_position_delta(
        sys_id=1, comp_id=196, seq=12,
        time_usec=4_000_000,
        time_delta_usec=33_333,
        angle_delta=[0.01, -0.02, 0.03],
        position_delta=[0.1, 0.2, -0.05],
        confidence=87.5,
    )
    msg = decoder(frame)
    assert msg.get_type() == "VISION_POSITION_DELTA"
    assert msg.time_usec == 4_000_000
    assert msg.time_delta_usec == 33_333
    assert math.isclose(msg.confidence, 87.5)
    for got, want in zip(msg.angle_delta, [0.01, -0.02, 0.03]):
        assert math.isclose(got, want, rel_tol=1e-6)
    for got, want in zip(msg.position_delta, [0.1, 0.2, -0.05]):
        assert math.isclose(got, want, rel_tol=1e-6)


# ─────────────────────── Registry / gate tables ───────────────────────


def test_registry_covers_all_vision_ids():
    for mid in (100, 101, 102, 106, 331, 11011):
        assert mid in MESSAGE_ID_TO_ENCODER
        assert mid in MESSAGE_NAMES
        assert mid in CRC_EXTRA_TABLE
        assert mid in ENCODER_CAPABILITY_GATES


def test_capability_routing_vision():
    # Pure sensor publishers ride on mavlink.write.
    assert ENCODER_CAPABILITY_GATES[100] == "mavlink.write"
    assert ENCODER_CAPABILITY_GATES[106] == "mavlink.write"
    # Estimator-pose injections need their own capability.
    for mid in (101, 102, 331, 11011):
        assert ENCODER_CAPABILITY_GATES[mid] == "estimator.pose.inject"


def test_crc_extra_table_matches_dialect():
    """Sanity-check against pymavlink's generated ardupilotmega dialect.
    Any drift here means the CRC_EXTRA constants in this package have
    fallen out of sync with the wire format, which would cause every
    frame to be rejected by a real receiver."""
    from pymavlink.dialects.v20 import ardupilotmega as ap

    for mid, crc in CRC_EXTRA_TABLE.items():
        cls = ap.mavlink_map[mid]
        assert crc == cls.crc_extra, f"CRC drift on {MESSAGE_NAMES[mid]}: ours={crc} dialect={cls.crc_extra}"
