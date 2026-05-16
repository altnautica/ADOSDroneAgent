"""Round-trip tests for rangefinder encoders."""

from __future__ import annotations

import math

import pytest

from ados.services.mavlink.encoders import (
    ENCODER_CAPABILITY_GATES,
    MESSAGE_ID_TO_ENCODER,
    encode_distance_sensor,
)


def test_distance_sensor_round_trip(decoder):
    frame = encode_distance_sensor(
        sys_id=1, comp_id=196, seq=3,
        time_boot_ms=123_456,
        min_distance=30,
        max_distance=4000,
        current_distance=275,
        type_=1,         # MAV_DISTANCE_SENSOR_ULTRASOUND
        id_=0,
        orientation=25,  # MAV_SENSOR_ROTATION_PITCH_270 (downward-facing)
        covariance=5,
        horizontal_fov=0.4,
        vertical_fov=0.4,
        quaternion=[1.0, 0.0, 0.0, 0.0],
        signal_quality=92,
    )
    assert frame[0] == 0xFD
    msg = decoder(frame)
    assert msg.get_type() == "DISTANCE_SENSOR"
    assert msg.time_boot_ms == 123_456
    assert msg.min_distance == 30
    assert msg.max_distance == 4000
    assert msg.current_distance == 275
    assert msg.type == 1
    assert msg.id == 0
    assert msg.orientation == 25
    assert msg.covariance == 5
    assert msg.signal_quality == 92
    assert math.isclose(msg.horizontal_fov, 0.4, rel_tol=1e-6)
    assert math.isclose(msg.vertical_fov, 0.4, rel_tol=1e-6)
    for got, want in zip(msg.quaternion, [1.0, 0.0, 0.0, 0.0]):
        assert math.isclose(got, want)


def test_distance_sensor_default_quaternion_round_trips(decoder):
    """Spec allows quaternion=[0,0,0,0] when orientation != CUSTOM."""
    frame = encode_distance_sensor(
        sys_id=1, comp_id=196, seq=0,
        time_boot_ms=100,
        min_distance=10, max_distance=1000, current_distance=120,
        type_=0, id_=0, orientation=0, covariance=0,
    )
    msg = decoder(frame)
    assert msg.current_distance == 120
    assert tuple(msg.quaternion) == (0.0, 0.0, 0.0, 0.0)


def test_distance_sensor_rejects_bad_quaternion():
    with pytest.raises(ValueError, match="quaternion"):
        encode_distance_sensor(
            sys_id=1, comp_id=196, seq=0,
            time_boot_ms=0,
            min_distance=0, max_distance=100, current_distance=50,
            type_=0, id_=0, orientation=0, covariance=0,
            quaternion=[1.0, 0.0],
        )


def test_distance_sensor_registry_entries():
    assert 132 in MESSAGE_ID_TO_ENCODER
    assert ENCODER_CAPABILITY_GATES[132] == "mavlink.write"
