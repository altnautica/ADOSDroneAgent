"""Vision surface constants: topics, RPC method names, wire versions."""

from __future__ import annotations

from ados.core.contracts import contract_version

# Topic the vision engine publishes frame descriptors on. Reserved to the
# host: plugins subscribe (with ``vision.frame.read``) but never publish here.
VISION_FRAME_TOPIC = "vision.frame"

# Topic detections are published on, labelled by model id.
VISION_DETECTION_TOPIC = "vision.detection"

# On-wire version of the detection-batch contract, sourced from the shared
# registry (crates/ados-protocol/contracts.toml) so the Python producer stamps
# the same integer the Rust ingress checks — a mismatch is rejected loudly at
# the host rather than silently mis-parsed.
_DETECTION_WIRE_VERSION = contract_version("vision.detection")
assert _DETECTION_WIRE_VERSION is not None, "vision.detection missing from the contract registry"

# Plugin RPC method names for the vision surface. The plugin host gates each on
# the matching capability before routing to the vision engine.
SUBSCRIBE_FRAMES = "vision.subscribe_frames"
SUBSCRIBE_DETECTIONS = "vision.subscribe_detections"
DESIGNATE_TRACK = "vision.designate_track"
REGISTER_MODEL = "vision.register_model"
INFER = "vision.infer"
PUBLISH_DETECTION = "vision.publish_detection"
DELIVER_FRAME = "vision.deliver"
DELIVER_DETECTION = "vision.deliver_detection"

# MAVLink component id a vision plugin registers as when it feeds pose to the
# flight controller. MAV_COMP_ID_VISUAL_INERTIAL_ODOMETRY (197): the FC tags
# the estimate as coming from a vision source rather than a peripheral or GCS.
VIO_COMPONENT_ID = 197

# Length of a MAVLink 6x6 pose cross-covariance upper triangle (21 row-major
# entries over the states x, y, z, roll, pitch, yaw).
POSE_COVARIANCE_LEN = 21

# A vision frame is sent under the agent's own system id by the host router.
# The component id is the VIO id; the host owns sequence numbering on the
# outbound link so the encoder seq is left at 0.
_DEFAULT_SYS_ID = 1
