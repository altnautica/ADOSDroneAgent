"""Setup helpers — origin and home-position injection.

These are companion-side outbound messages that configure the vehicle
state machine on first power-up or after a reset. They both use the
``mavlink.write`` capability gate (companion-to-FC configuration).

NB on message ids:

* SET_GPS_GLOBAL_ORIGIN is id 48 in common.xml.
* SET_HOME_POSITION is id 243 in common.xml — note this is **243**, not
  49. Id 49 is the (different) GPS_GLOBAL_ORIGIN response message that
  the FC emits, not a setter.
"""

from __future__ import annotations

import struct
from typing import Final, Sequence

from ._framing import pack_v2

MSG_ID_SET_GPS_GLOBAL_ORIGIN: Final = 48
MSG_ID_SET_HOME_POSITION: Final = 243

CRC_SET_GPS_GLOBAL_ORIGIN: Final = 41
CRC_SET_HOME_POSITION: Final = 85

# Wire order: latitude, longitude, altitude, target_system, time_usec
_PACK_SET_GPS_GLOBAL_ORIGIN = struct.Struct("<iiiBQ").pack

# Wire order: latitude, longitude, altitude, x, y, z, q[4],
# approach_x, approach_y, approach_z, target_system, time_usec
_PACK_SET_HOME_POSITION = struct.Struct("<iiifff4ffffBQ").pack


def encode_set_gps_global_origin(
    sys_id: int,
    comp_id: int,
    seq: int,
    target_system: int,
    latitude: int,
    longitude: int,
    altitude: int,
    time_usec: int = 0,
) -> bytes:
    """Build a SET_GPS_GLOBAL_ORIGIN (48) v2 frame.

    Latitude and longitude are int32 in degE7 (degrees * 1e7).
    Altitude is int32 millimetres above MSL.
    """
    payload = _PACK_SET_GPS_GLOBAL_ORIGIN(
        latitude, longitude, altitude, target_system, time_usec,
    )
    return pack_v2(
        msg_id=MSG_ID_SET_GPS_GLOBAL_ORIGIN, crc_extra=CRC_SET_GPS_GLOBAL_ORIGIN,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )


def encode_set_home_position(
    sys_id: int,
    comp_id: int,
    seq: int,
    target_system: int,
    latitude: int,
    longitude: int,
    altitude: int,
    x: float,
    y: float,
    z: float,
    q: Sequence[float],
    approach_x: float,
    approach_y: float,
    approach_z: float,
    time_usec: int = 0,
) -> bytes:
    """Build a SET_HOME_POSITION (243) v2 frame.

    `q` is the surface-orientation quaternion (length 4) used by the
    landing logic to capture heading and terrain slope. Approach is
    the 3D vector the vehicle flies along before its landing sequence.
    """
    if len(q) != 4:
        raise ValueError(f"q must have length 4, got {len(q)}")
    payload = _PACK_SET_HOME_POSITION(
        latitude, longitude, altitude,
        x, y, z,
        q[0], q[1], q[2], q[3],
        approach_x, approach_y, approach_z,
        target_system, time_usec,
    )
    return pack_v2(
        msg_id=MSG_ID_SET_HOME_POSITION, crc_extra=CRC_SET_HOME_POSITION,
        payload=payload, sys_id=sys_id, comp_id=comp_id, seq=seq,
    )
