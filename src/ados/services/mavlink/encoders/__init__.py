"""Pure-function MAVLink v2 encoders for messages the plugin host emits.

Each encoder takes plain-Python primitives (and short numeric sequences
for fixed-length arrays such as quaternions and covariance matrices) and
returns a complete on-wire MAVLink v2 frame as ``bytes``. The encoders
hold no state — sequence numbers are passed in by the caller so SEQ
tracking can live wherever it makes sense for the host (per link, per
``(sys_id, comp_id)`` pair, or globally).

Three taxonomies are exported alongside the functions:

* :data:`MESSAGE_ID_TO_ENCODER` — runtime dispatch for callers that have
  a message id and a payload dict and want a frame.
* :data:`ENCODER_CAPABILITY_GATES` — message id to capability name. The
  plugin IPC dispatcher consults this table before letting a plugin's
  ``mavlink.send`` call reach the wire.
* :data:`MESSAGE_NAMES` — message id to canonical name, useful for logs
  and error strings.
"""

from __future__ import annotations

from typing import Callable, Dict, Final

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
    "ENCODER_CAPABILITY_GATES",
    "MESSAGE_NAMES",
    "CRC_EXTRA_TABLE",
]

# Capability names used by the plugin host. Kept as string constants so
# this module has no runtime import dependency on the capability catalog
# module — the canonical catalog at ``ados.plugins.capabilities`` is
# checked at the IPC dispatcher layer, not here.
_CAP_MAVLINK_WRITE: Final = "mavlink.write"
_CAP_ESTIMATOR_POSE_INJECT: Final = "estimator.pose.inject"


#: Dispatch table from message id to its encoder function. A caller
#: with a typed payload dict and a message id can do
#: ``frame = MESSAGE_ID_TO_ENCODER[msg_id](sys_id, comp_id, seq, **payload)``.
MESSAGE_ID_TO_ENCODER: Dict[int, Callable[..., bytes]] = {
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


#: Capability gate per message id. The plugin host's IPC dispatcher
#: rejects ``mavlink.send`` calls whose msg id maps to a capability the
#: calling plugin has not been granted. Estimator-injection messages
#: route through their own capability so a plugin can be allowed to
#: stream vision odometry without also being able to set arbitrary
#: parameters or trigger commands.
ENCODER_CAPABILITY_GATES: Dict[int, str] = {
    # Sensor publishers — "I observed something, here it is."
    MSG_ID_OPTICAL_FLOW: _CAP_MAVLINK_WRITE,
    MSG_ID_OPTICAL_FLOW_RAD: _CAP_MAVLINK_WRITE,
    MSG_ID_DISTANCE_SENSOR: _CAP_MAVLINK_WRITE,
    # Estimator-pose injection — touches the FC's nav state directly.
    MSG_ID_VISION_POSITION_ESTIMATE: _CAP_ESTIMATOR_POSE_INJECT,
    MSG_ID_GLOBAL_VISION_POSITION_ESTIMATE: _CAP_ESTIMATOR_POSE_INJECT,
    MSG_ID_ODOMETRY: _CAP_ESTIMATOR_POSE_INJECT,
    MSG_ID_VISION_POSITION_DELTA: _CAP_ESTIMATOR_POSE_INJECT,
    # Companion-side setup. Mutates origin and home, so this is
    # ``mavlink.write`` rather than estimator-inject.
    MSG_ID_SET_GPS_GLOBAL_ORIGIN: _CAP_MAVLINK_WRITE,
    MSG_ID_SET_HOME_POSITION: _CAP_MAVLINK_WRITE,
}


#: Message id to canonical name. Mirrors the dialect XML msgname so log
#: lines and error strings can render a human-readable label.
MESSAGE_NAMES: Dict[int, str] = {
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
CRC_EXTRA_TABLE: Dict[int, int] = {
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
