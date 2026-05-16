"""Round-trip tests for the setup encoders (origin + home position)."""

from __future__ import annotations

import math

import pytest

from ados.services.mavlink.encoders import (
    ENCODER_CAPABILITY_GATES,
    MESSAGE_ID_TO_ENCODER,
    encode_set_gps_global_origin,
    encode_set_home_position,
)


def test_set_gps_global_origin_round_trip(decoder):
    # Bangalore-ish coordinates in degE7.
    frame = encode_set_gps_global_origin(
        sys_id=255, comp_id=190, seq=1,
        target_system=1,
        latitude=128_000_000,
        longitude=775_000_000,
        altitude=920_000,  # mm
        time_usec=987_654_321,
    )
    assert frame[0] == 0xFD
    msg = decoder(frame)
    assert msg.get_type() == "SET_GPS_GLOBAL_ORIGIN"
    assert msg.target_system == 1
    assert msg.latitude == 128_000_000
    assert msg.longitude == 775_000_000
    assert msg.altitude == 920_000
    assert msg.time_usec == 987_654_321


def test_set_gps_global_origin_msg_id_byte():
    """msg id 48 must fit in a single low byte; upper two bytes zero."""
    frame = encode_set_gps_global_origin(
        sys_id=1, comp_id=1, seq=0,
        target_system=1, latitude=0, longitude=0, altitude=0,
    )
    assert frame[7] == 48
    assert frame[8] == 0
    assert frame[9] == 0


def test_set_home_position_round_trip(decoder):
    q = [0.70710677, 0.0, 0.0, 0.70710677]  # 90° yaw quaternion
    frame = encode_set_home_position(
        sys_id=255, comp_id=190, seq=2,
        target_system=1,
        latitude=128_500_000,
        longitude=775_500_000,
        altitude=900_000,
        x=5.0, y=-2.5, z=-1.25,
        q=q,
        approach_x=10.0, approach_y=0.0, approach_z=-3.0,
        time_usec=1_234_567,
    )
    msg = decoder(frame)
    assert msg.get_type() == "SET_HOME_POSITION"
    assert msg.target_system == 1
    assert msg.latitude == 128_500_000
    assert msg.longitude == 775_500_000
    assert msg.altitude == 900_000
    assert math.isclose(msg.x, 5.0)
    assert math.isclose(msg.y, -2.5)
    assert math.isclose(msg.z, -1.25)
    assert math.isclose(msg.approach_x, 10.0)
    assert math.isclose(msg.approach_z, -3.0)
    for got, want in zip(msg.q, q):
        assert math.isclose(got, want, rel_tol=1e-6)
    assert msg.time_usec == 1_234_567


def test_set_home_position_uses_id_243():
    """Guards against the common mistake of using id 49 (which is
    GPS_GLOBAL_ORIGIN response, NOT a setter)."""
    frame = encode_set_home_position(
        sys_id=1, comp_id=1, seq=0,
        target_system=1, latitude=0, longitude=0, altitude=0,
        x=0, y=0, z=0, q=[1.0, 0, 0, 0],
        approach_x=0, approach_y=0, approach_z=0,
    )
    # msg id field is 3 LE bytes starting at offset 7.
    msg_id = frame[7] | (frame[8] << 8) | (frame[9] << 16)
    assert msg_id == 243


def test_set_home_position_rejects_bad_q():
    with pytest.raises(ValueError, match="q"):
        encode_set_home_position(
            sys_id=1, comp_id=1, seq=0,
            target_system=1, latitude=0, longitude=0, altitude=0,
            x=0, y=0, z=0, q=[1.0],
            approach_x=0, approach_y=0, approach_z=0,
        )


def test_setup_registry_entries():
    assert 48 in MESSAGE_ID_TO_ENCODER
    assert 243 in MESSAGE_ID_TO_ENCODER
    assert ENCODER_CAPABILITY_GATES[48] == "mavlink.write"
    assert ENCODER_CAPABILITY_GATES[243] == "mavlink.write"
