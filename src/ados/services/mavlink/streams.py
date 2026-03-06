"""Data stream management — request message intervals from FC."""

from __future__ import annotations

from pymavlink import mavutil

from ados.core.logging import get_logger

log = get_logger("mavlink.streams")

# Message ID -> desired rate in Hz
DEFAULT_STREAM_RATES: dict[int, float] = {
    0: 1.0,      # HEARTBEAT
    30: 10.0,    # ATTITUDE
    33: 5.0,     # GLOBAL_POSITION_INT
    1: 2.0,      # SYS_STATUS
    24: 2.0,     # GPS_RAW_INT
    74: 4.0,     # VFR_HUD
    147: 1.0,    # BATTERY_STATUS
    65: 4.0,     # RC_CHANNELS
}


def request_data_streams(conn: mavutil.mavlink_connection) -> None:
    """Send SET_MESSAGE_INTERVAL for each desired message at the specified rate."""
    target_system = conn.target_system
    target_component = conn.target_component

    for msg_id, rate_hz in DEFAULT_STREAM_RATES.items():
        interval_us = int(1_000_000 / rate_hz)
        conn.mav.command_long_send(
            target_system,
            target_component,
            mavutil.mavlink.MAV_CMD_SET_MESSAGE_INTERVAL,
            0,          # confirmation
            msg_id,     # param1: message ID
            interval_us,  # param2: interval in microseconds
            0, 0, 0, 0,  # param3-6: unused
            0,            # param7: response target (0 = flight stack default)
        )

    log.info("streams_requested", count=len(DEFAULT_STREAM_RATES))
