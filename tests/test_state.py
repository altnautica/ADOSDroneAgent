"""Tests for VehicleState updates from MAVLink messages."""

from __future__ import annotations

from ados.services.mavlink.state import VehicleState


def test_heartbeat_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "HEARTBEAT",
        type=2,
        autopilot=3,
        base_mode=0x80 | 0x01,  # armed
        custom_mode=5,
        system_status=4,
    )
    state.update_from_message(msg)

    assert state.mav_type == 2
    assert state.autopilot == 3
    assert state.armed is True
    assert state.custom_mode == 5
    assert state.last_heartbeat != ""


def test_global_position_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "GLOBAL_POSITION_INT",
        lat=127800000,
        lon=776800000,
        alt=50000,
        relative_alt=30000,
        vx=100,
        vy=200,
        vz=-50,
        hdg=18000,
    )
    state.update_from_message(msg)

    assert abs(state.lat - 12.78) < 0.001
    assert abs(state.lon - 77.68) < 0.001
    assert abs(state.alt_msl - 50.0) < 0.1
    assert abs(state.alt_rel - 30.0) < 0.1
    assert abs(state.heading - 180.0) < 0.1


def test_attitude_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "ATTITUDE",
        roll=0.1,
        pitch=-0.2,
        yaw=1.5,
        rollspeed=0.01,
        pitchspeed=-0.02,
        yawspeed=0.0,
    )
    state.update_from_message(msg)

    assert abs(state.roll - 0.1) < 0.001
    assert abs(state.pitch - (-0.2)) < 0.001
    assert abs(state.yaw - 1.5) < 0.001


def test_sys_status_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "SYS_STATUS",
        voltage_battery=22400,
        current_battery=1500,
        battery_remaining=75,
        onboard_control_sensors_health=0xFFFF,
    )
    state.update_from_message(msg)

    assert abs(state.voltage_battery - 22.4) < 0.1
    assert abs(state.current_battery - 15.0) < 0.1
    assert state.battery_remaining == 75


def test_gps_raw_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "GPS_RAW_INT",
        fix_type=3,
        satellites_visible=12,
        eph=150,
        epv=200,
    )
    state.update_from_message(msg)

    assert state.gps_fix_type == 3
    assert state.gps_satellites == 12
    assert abs(state.gps_eph - 1.5) < 0.01


def test_vfr_hud_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "VFR_HUD",
        airspeed=12.5,
        groundspeed=11.3,
        climb=0.5,
        throttle=45,
    )
    state.update_from_message(msg)

    assert abs(state.airspeed - 12.5) < 0.1
    assert state.throttle == 45


def test_rc_channels_update(mock_mavlink_msg):
    state = VehicleState()
    channels = {f"chan{i}_raw": 1500 + i * 10 for i in range(1, 19)}
    channels["rssi"] = 200
    msg = mock_mavlink_msg("RC_CHANNELS", **channels)
    state.update_from_message(msg)

    assert state.rc_channels[0] == 1510
    assert state.rc_channels[17] == 1680
    assert state.rc_rssi == 200


def test_param_value_update(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "PARAM_VALUE",
        param_id="BATT_CAPACITY\x00\x00\x00",
        param_value=5200.0,
        param_count=350,
    )
    state.update_from_message(msg)

    assert "BATT_CAPACITY" in state.params
    assert state.params["BATT_CAPACITY"] == 5200.0
    assert state.param_count == 350


def test_to_dict(mock_mavlink_msg):
    state = VehicleState()
    msg = mock_mavlink_msg(
        "HEARTBEAT",
        type=2, autopilot=3, base_mode=0x80,
        custom_mode=0, system_status=4,
    )
    state.update_from_message(msg)
    d = state.to_dict()

    assert "armed" in d
    assert "position" in d
    assert "battery" in d
    assert "gps" in d
    assert "rc" in d
