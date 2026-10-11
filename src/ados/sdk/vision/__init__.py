"""Vision surface: frame subscription, model registration, inference, and
detection publishing, plus the visual-odometry pose helper.

A vision plugin reaches the agent's vision engine over the same plugin RPC
wire as every other surface, but frames themselves never ride the RPC
envelope. The engine writes normalized frames into a shared-memory ring and
publishes a small :class:`FrameDescriptor` on the ``vision.frame`` topic; the
host delivers each descriptor to a subscriber as a ``vision.deliver`` event.
This client resolves a descriptor to pixels by memory-mapping the named
``/dev/shm`` ring read-only and reading the descriptor's slot through the
per-slot seqlock the frame-transport contract defines, dropping any torn or
stale read (latest-wins).

Detections and model metadata are small structured payloads, so they ride the
RPC envelope directly through the methods the host gates on the vision
capabilities.

The client gates nothing itself; the host enforces ``vision.frame.read``,
``vision.model.register``, and ``vision.detection.publish``.

The wire shapes here mirror the shared frame-transport contract byte for byte:
the msgpack field names are identical so a Python plugin and a Rust plugin
read and publish the same wire. The shared-memory ring layout (16-byte ring
header; per-slot ``seq_begin`` / ``byte_len`` / data / ``seq_end`` seqlock) is
the same one the engine writes and the Rust client reads.
"""

from ados.core.logging import get_logger
from ados.sdk.vision._constants import (
    _DEFAULT_SYS_ID as _DEFAULT_SYS_ID,
)
from ados.sdk.vision._constants import (
    _DETECTION_WIRE_VERSION as _DETECTION_WIRE_VERSION,
)
from ados.sdk.vision._constants import (
    DELIVER_DETECTION,
    DELIVER_FRAME,
    DESIGNATE_TRACK,
    INFER,
    POSE_COVARIANCE_LEN,
    PUBLISH_DETECTION,
    REGISTER_MODEL,
    SUBSCRIBE_DETECTIONS,
    SUBSCRIBE_FRAMES,
    VIO_COMPONENT_ID,
    VISION_DETECTION_TOPIC,
    VISION_FRAME_TOPIC,
)
from ados.sdk.vision._ring import _decode_descriptor as _decode_descriptor
from ados.sdk.vision._ring import _decode_infer_reply as _decode_infer_reply
from ados.sdk.vision._ring import _map_ring as _map_ring
from ados.sdk.vision._ring import _MappedRing as _MappedRing
from ados.sdk.vision._ring import _RingResolver as _RingResolver
from ados.sdk.vision._ring import _shm_dir as _shm_dir
from ados.sdk.vision.client import VisionClient
from ados.sdk.vision.detections import (
    BoundingBox,
    Detection,
    DetectionBatch,
    DetectionHead,
    Keypoint,
    ModelExecution,
    ModelKind,
    ModelMetadata,
)
from ados.sdk.vision.frames import (
    DetectionCallback,
    Frame,
    FrameCallback,
    FrameDescriptor,
    FrameFormat,
    RingLayout,
    read_slot,
    write_slot,
)
from ados.sdk.vision.pose import Odometry, Pose

log = get_logger("sdk.vision")

__all__ = [
    "FrameFormat",
    "FrameDescriptor",
    "Frame",
    "FrameCallback",
    "DetectionCallback",
    "RingLayout",
    "write_slot",
    "read_slot",
    "ModelKind",
    "ModelExecution",
    "DetectionHead",
    "ModelMetadata",
    "BoundingBox",
    "Keypoint",
    "Detection",
    "DetectionBatch",
    "Pose",
    "Odometry",
    "VisionClient",
    "VISION_FRAME_TOPIC",
    "VISION_DETECTION_TOPIC",
    "SUBSCRIBE_FRAMES",
    "SUBSCRIBE_DETECTIONS",
    "DESIGNATE_TRACK",
    "REGISTER_MODEL",
    "INFER",
    "PUBLISH_DETECTION",
    "DELIVER_FRAME",
    "DELIVER_DETECTION",
    "VIO_COMPONENT_ID",
    "POSE_COVARIANCE_LEN",
]
