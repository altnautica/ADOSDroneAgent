"""Pure-function MAVLink v2 encoders for messages the plugin host emits.

Each encoder takes plain-Python primitives (and short numeric sequences
for fixed-length arrays such as quaternions and covariance matrices) and
returns a complete on-wire MAVLink v2 frame as ``bytes``. The encoders
hold no state — sequence numbers are passed in by the caller so SEQ
tracking can live wherever it makes sense for the host (per link, per
``(sys_id, comp_id)`` pair, or globally).

Two taxonomies are exported alongside the functions:

* :data:`MESSAGE_ID_TO_ENCODER` — runtime dispatch for callers that have
  a message id and a payload dict and want a frame.
* :data:`MESSAGE_NAMES` — message id to canonical name, useful for logs
  and error strings.

Which capability a plugin needs to send a given message id is decided by
the plugin host, not by this package.
"""

from __future__ import annotations

from collections.abc import Callable

from .rangefinder import (
    CRC_DISTANCE_SENSOR,
    MSG_ID_DISTANCE_SENSOR,
    encode_distance_sensor,
)
from .setup import (
    CRC_SET_GPS_GLOBAL_ORIGIN,
    CRC_SET_HOME_POSITION,
    MSG_ID_SET_GPS_GLOBAL_ORIGIN,
    MSG_ID_SET_HOME_POSITION,
    encode_set_gps_global_origin,
    encode_set_home_position,
)
from .vision import (
    CRC_GLOBAL_VISION_POSITION_ESTIMATE,
    CRC_ODOMETRY,
    CRC_OPTICAL_FLOW,
    CRC_OPTICAL_FLOW_RAD,
    CRC_VISION_POSITION_DELTA,
    CRC_VISION_POSITION_ESTIMATE,
    MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE,
    MSG_ID_ODOMETRY,
    MSG_ID_OPTICAL_FLOW,
    MSG_ID_OPTICAL_FLOW_RAD,
    MSG_ID_VISION_POSITION_DELTA,
    MSG_ID_VISION_POSITION_ESTIMATE,
    encode_global_vision_position_estimate,
    encode_odometry,
    encode_optical_flow,
    encode_optical_flow_rad,
    encode_vision_position_delta,
    encode_vision_position_estimate,
)

__all__ = [
    # Vision
    "encode_optical_flow",
    "encode_optical_flow_rad",
    "encode_vision_position_estimate",
    "encode_global_vision_position_estimate",
    "encode_odometry",
    "encode_vision_position_delta",
    # Rangefinder
    "encode_distance_sensor",
    # Setup
    "encode_set_gps_global_origin",
    "encode_set_home_position",
    # Tables
    "MESSAGE_ID_TO_ENCODER",
    "MESSAGE_NAMES",
    "CRC_EXTRA_TABLE",
]


#: Dispatch table from message id to its encoder function. A caller
#: with a typed payload dict and a message id can do
#: ``frame = MESSAGE_ID_TO_ENCODER[msg_id](sys_id, comp_id, seq, **payload)``.
MESSAGE_ID_TO_ENCODER: dict[int, Callable[..., bytes]] = {
    MSG_ID_OPTICAL_FLOW: encode_optical_flow,
    MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE: encode_global_vision_position_estimate,
    MSG_ID_VISION_POSITION_ESTIMATE: encode_vision_position_estimate,
    MSG_ID_OPTICAL_FLOW_RAD: encode_optical_flow_rad,
    MSG_ID_DISTANCE_SENSOR: encode_distance_sensor,
    MSG_ID_ODOMETRY: encode_odometry,
    MSG_ID_VISION_POSITION_DELTA: encode_vision_position_delta,
    MSG_ID_SET_GPS_GLOBAL_ORIGIN: encode_set_gps_global_origin,
    MSG_ID_SET_HOME_POSITION: encode_set_home_position,
}


#: Message id to canonical name. Mirrors the dialect XML msgname so log
#: lines and error strings can render a human-readable label.
MESSAGE_NAMES: dict[int, str] = {
    MSG_ID_OPTICAL_FLOW: "OPTICAL_FLOW",
    MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE: "GLOBAL_VISION_POSITION_ESTIMATE",
    MSG_ID_VISION_POSITION_ESTIMATE: "VISION_POSITION_ESTIMATE",
    MSG_ID_OPTICAL_FLOW_RAD: "OPTICAL_FLOW_RAD",
    MSG_ID_DISTANCE_SENSOR: "DISTANCE_SENSOR",
    MSG_ID_ODOMETRY: "ODOMETRY",
    MSG_ID_VISION_POSITION_DELTA: "VISION_POSITION_DELTA",
    MSG_ID_SET_GPS_GLOBAL_ORIGIN: "SET_GPS_GLOBAL_ORIGIN",
    MSG_ID_SET_HOME_POSITION: "SET_HOME_POSITION",
}


#: Message id to dialect CRC_EXTRA byte. Exported for diagnostic tools
#: and for tests that want to assert frame integrity without going
#: through a full decoder.
CRC_EXTRA_TABLE: dict[int, int] = {
    MSG_ID_OPTICAL_FLOW: CRC_OPTICAL_FLOW,
    MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE: CRC_GLOBAL_VISION_POSITION_ESTIMATE,
    MSG_ID_VISION_POSITION_ESTIMATE: CRC_VISION_POSITION_ESTIMATE,
    MSG_ID_OPTICAL_FLOW_RAD: CRC_OPTICAL_FLOW_RAD,
    MSG_ID_DISTANCE_SENSOR: CRC_DISTANCE_SENSOR,
    MSG_ID_ODOMETRY: CRC_ODOMETRY,
    MSG_ID_VISION_POSITION_DELTA: CRC_VISION_POSITION_DELTA,
    MSG_ID_SET_GPS_GLOBAL_ORIGIN: CRC_SET_GPS_GLOBAL_ORIGIN,
    MSG_ID_SET_HOME_POSITION: CRC_SET_HOME_POSITION,
}
