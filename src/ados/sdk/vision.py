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

from __future__ import annotations

import contextlib
import enum
import mmap
import struct
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

import msgpack

from ados.core.logging import get_logger
from ados.services.mavlink.encoders import (
    encode_odometry,
    encode_vision_position_estimate,
)

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient

log = get_logger("sdk.vision")

# Topic the vision engine publishes frame descriptors on. Reserved to the
# host: plugins subscribe (with ``vision.frame.read``) but never publish here.
VISION_FRAME_TOPIC = "vision.frame"

# Topic detections are published on, labelled by model id.
VISION_DETECTION_TOPIC = "vision.detection"

# Plugin RPC method names for the vision surface. The plugin host gates each on
# the matching capability before routing to the vision engine.
SUBSCRIBE_FRAMES = "vision.subscribe_frames"
REGISTER_MODEL = "vision.register_model"
INFER = "vision.infer"
PUBLISH_DETECTION = "vision.publish_detection"
DELIVER_FRAME = "vision.deliver"

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


# ---------------------------------------------------------------------------
# Frame format + descriptor.
# ---------------------------------------------------------------------------


class FrameFormat(str, enum.Enum):
    """Normalized pixel format of a frame in the ring. The engine downscales
    and converts the camera's native format to one of these before publishing.

    A ``str`` enum so the member value is exactly the lowercase string the
    msgpack wire carries, matching the Rust ``lowercase`` rename.
    """

    RGB24 = "rgb24"
    """Packed 24-bit RGB, 3 bytes per pixel."""
    NV12 = "nv12"
    """Semi-planar YUV 4:2:0 (Y plane then interleaved UV), 1.5 bytes/pixel."""
    YUV420P = "yuv420p"
    """Planar YUV 4:2:0 (Y, then U, then V), 1.5 bytes per pixel."""

    def frame_bytes(self, width: int, height: int) -> int:
        """Exact byte length of one ``width`` x ``height`` frame in this format.

        The 4:2:0 formats require even dimensions; callers normalize to even
        width/height before sizing a ring.
        """
        px = int(width) * int(height)
        if self is FrameFormat.RGB24:
            return px * 3
        # Y plane (px) + chroma (px // 2) = px * 3 // 2.
        return px + px // 2


@dataclass(frozen=True)
class FrameDescriptor:
    """The small message published on ``vision.frame``. It names the ring slot
    a consumer should read and carries the ``seq`` that the per-slot seqlock
    must still hold for the read to be valid.

    The msgpack field names match the shared frame-transport contract exactly
    so a descriptor encoded here decodes in Rust and vice versa.
    """

    camera_id: str
    """Source camera id. Lets a consumer filter by camera."""
    frame_id: int
    """Monotonic frame counter for this camera, starting at 1."""
    ts_ms: int
    """Capture time in milliseconds (the clock the engine timestamps with)."""
    width: int
    height: int
    format: FrameFormat
    shm_name: str
    """``/dev/shm`` name of the ring this frame lives in (one ring per camera)."""
    slot: int
    """Slot index within the ring holding this frame's pixels."""
    seq: int
    """Ring sequence stamped on the slot; re-checked against the slot's
    seqlock after copying. A mismatch means a torn read and the frame is
    dropped."""
    byte_len: int
    """Length of the valid pixel bytes in the slot (``format.frame_bytes``)."""

    def to_dict(self) -> dict[str, Any]:
        """The msgpack-named mapping. ``format`` is the lowercase string."""
        return {
            "camera_id": self.camera_id,
            "frame_id": int(self.frame_id),
            "ts_ms": int(self.ts_ms),
            "width": int(self.width),
            "height": int(self.height),
            "format": FrameFormat(self.format).value,
            "shm_name": self.shm_name,
            "slot": int(self.slot),
            "seq": int(self.seq),
            "byte_len": int(self.byte_len),
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> FrameDescriptor:
        return cls(
            camera_id=str(raw["camera_id"]),
            frame_id=int(raw["frame_id"]),
            ts_ms=int(raw["ts_ms"]),
            width=int(raw["width"]),
            height=int(raw["height"]),
            format=FrameFormat(str(raw["format"])),
            shm_name=str(raw["shm_name"]),
            slot=int(raw["slot"]),
            seq=int(raw["seq"]),
            byte_len=int(raw["byte_len"]),
        )

    def to_msgpack(self) -> bytes:
        return msgpack.packb(self.to_dict(), use_bin_type=True)

    @classmethod
    def from_msgpack(cls, blob: bytes) -> FrameDescriptor:
        raw = msgpack.unpackb(blob, raw=False)
        if not isinstance(raw, dict):
            raise ValueError("frame descriptor is not a msgpack mapping")
        return cls.from_dict(raw)


@dataclass(frozen=True)
class Frame:
    """A resolved camera frame: the descriptor the engine published plus the
    pixel bytes read out of the shared-memory ring it named.

    ``len(pixels)`` equals ``descriptor.byte_len`` and is the valid pixel data
    for ``descriptor.width`` x ``descriptor.height`` in ``descriptor.format``.
    """

    descriptor: FrameDescriptor
    pixels: bytes


# A callback invoked once per resolved frame. A frame the ring could not
# resolve (torn or stale read, or a ring that vanished) is dropped silently and
# the callback does not fire for it.
FrameCallback = Callable[[Frame], Awaitable[None] | None]


# ---------------------------------------------------------------------------
# Ring layout + seqlock read (mirrors the shared frame-transport contract).
# ---------------------------------------------------------------------------


# "ADV1" — ADOS vision ring, version 1.
_RING_MAGIC = 0x41445631
_RING_VERSION = 1
_HEADER_LEN = 16
# seq_begin (u64) + byte_len (u32) + pad (u32).
_SLOT_HEADER_LEN = 16
# seq_end (u64).
_SLOT_TRAILER_LEN = 8

_U64 = struct.Struct("<Q")
_U32 = struct.Struct("<I")
_HEADER = struct.Struct("<IHHII")


@dataclass(frozen=True)
class RingLayout:
    """Memory layout of a single-writer, many-reader frame ring.

    Layout (all integers little-endian)::

        [ ring header ]  16 bytes: magic, version, slot_count, slot_bytes, pad
        [ slot 0      ]  seq_begin:u64 | byte_len:u32 | pad:u32 | data | seq_end:u64
        [ slot 1      ]
          ...

    The writer stores ``seq_begin``, then the data and length, then ``seq_end``;
    a reader loads ``seq_end``, copies the data, then re-checks ``seq_begin`` and
    ``seq_end`` against the descriptor's ``seq``. If either differs the read was
    torn by a slot recycle and is discarded.
    """

    slot_count: int
    slot_bytes: int

    @classmethod
    def for_frame(
        cls, slot_count: int, width: int, height: int, fmt: FrameFormat
    ) -> RingLayout:
        return cls(
            slot_count=int(slot_count),
            slot_bytes=fmt.frame_bytes(width, height),
        )

    def slot_stride(self) -> int:
        return _SLOT_HEADER_LEN + self.slot_bytes + _SLOT_TRAILER_LEN

    def total_len(self) -> int:
        return _HEADER_LEN + self.slot_count * self.slot_stride()

    def slot_offset(self, slot: int) -> int:
        return _HEADER_LEN + slot * self.slot_stride()

    def write_header(self, region: bytearray | memoryview) -> None:
        """Write the ring header at the front of a freshly created region."""
        if len(region) < self.total_len():
            raise ValueError(
                f"shared region is {len(region)} bytes, layout needs "
                f"{self.total_len()}"
            )
        region[0:_HEADER_LEN] = _HEADER.pack(
            _RING_MAGIC,
            _RING_VERSION,
            self.slot_count & 0xFFFF,
            self.slot_bytes & 0xFFFFFFFF,
            0,
        )

    @classmethod
    def read_header(cls, region: bytes | memoryview) -> RingLayout | None:
        """Read the layout a writer recorded in a region's header. ``None`` if
        the region is too small or the magic/version do not match."""
        if len(region) < _HEADER_LEN:
            return None
        magic, version, slot_count, slot_bytes, _pad = _HEADER.unpack(
            bytes(region[0:_HEADER_LEN])
        )
        if magic != _RING_MAGIC or version != _RING_VERSION:
            return None
        return cls(slot_count=slot_count, slot_bytes=slot_bytes)


def write_slot(
    region: bytearray | memoryview,
    layout: RingLayout,
    slot: int,
    seq: int,
    data: bytes,
) -> None:
    """Write one frame into ``slot`` of the ring, stamping it with ``seq``.

    The single writer chooses ``slot = seq % slot_count`` and calls this once
    per captured frame, then publishes the matching :class:`FrameDescriptor`.
    """
    if not 0 <= slot < layout.slot_count:
        raise ValueError(
            f"slot {slot} out of range (ring has {layout.slot_count} slots)"
        )
    cap = layout.slot_bytes
    if len(data) > cap:
        raise ValueError(
            f"payload of {len(data)} bytes exceeds slot capacity {cap}"
        )
    if len(region) < layout.total_len():
        raise ValueError(
            f"shared region is {len(region)} bytes, layout needs "
            f"{layout.total_len()}"
        )

    base = layout.slot_offset(slot)
    data_off = base + _SLOT_HEADER_LEN
    trailer_off = data_off + cap

    # seq_begin first (marks the slot as being written for this seq).
    region[base : base + 8] = _U64.pack(seq)
    region[base + 8 : base + 12] = _U32.pack(len(data))
    region[base + 12 : base + 16] = _U32.pack(0)
    region[data_off : data_off + len(data)] = data
    # seq_end last (commits the write).
    region[trailer_off : trailer_off + 8] = _U64.pack(seq)


def read_slot(
    region: bytes | memoryview,
    layout: RingLayout,
    slot: int,
    expected_seq: int,
) -> bytes | None:
    """Read the frame a :class:`FrameDescriptor` points at, validating the
    seqlock.

    Returns ``None`` if the slot no longer holds ``expected_seq`` (the writer
    recycled it) so the caller drops that frame and waits for the next
    descriptor.
    """
    if not 0 <= slot < layout.slot_count:
        raise ValueError(
            f"slot {slot} out of range (ring has {layout.slot_count} slots)"
        )
    if len(region) < layout.total_len():
        raise ValueError(
            f"shared region is {len(region)} bytes, layout needs "
            f"{layout.total_len()}"
        )

    cap = layout.slot_bytes
    base = layout.slot_offset(slot)
    data_off = base + _SLOT_HEADER_LEN
    trailer_off = data_off + cap

    # Load the trailer (committed marker) first.
    (seq_end,) = _U64.unpack(bytes(region[trailer_off : trailer_off + 8]))
    if seq_end != expected_seq:
        return None
    (byte_len,) = _U32.unpack(bytes(region[base + 8 : base + 12]))
    if byte_len > cap:
        return None
    data = bytes(region[data_off : data_off + byte_len])
    # Re-check both guards: a writer that recycled this slot mid-copy moves the
    # seq forward, so a stale begin or a changed end means the copy was torn.
    (seq_begin,) = _U64.unpack(bytes(region[base : base + 8]))
    (seq_end2,) = _U64.unpack(bytes(region[trailer_off : trailer_off + 8]))
    if seq_begin != expected_seq or seq_end2 != expected_seq:
        return None
    return data


# ---------------------------------------------------------------------------
# Model + detection contracts (small payloads, ride the RPC envelope).
# ---------------------------------------------------------------------------


class ModelKind(str, enum.Enum):
    """What a model produces, so consumers know how to read its output."""

    DETECTION = "detection"
    SEGMENTATION = "segmentation"
    CLASSIFICATION = "classification"
    TRACKING = "tracking"


class ModelExecution(str, enum.Enum):
    """How a registered model is executed."""

    ENGINE_RUN = "engine_run"
    """The engine loads the model file and runs it on the shared backend, then
    publishes detections itself."""
    PLUGIN_SIDE = "plugin_side"
    """The plugin runs the model and publishes detections (or calls ``infer``)."""


@dataclass(frozen=True)
class ModelMetadata:
    """Metadata a plugin supplies when registering a model.

    The msgpack field names match the shared contract: ``id``, ``kind``,
    ``execution``, ``input_width``, ``input_height``, ``input_format``,
    ``output_classes``, ``model_path``.
    """

    id: str
    """Reverse-DNS-ish unique model id (e.g. ``com.example.weeds``)."""
    kind: ModelKind
    execution: ModelExecution
    input_width: int
    input_height: int
    input_format: FrameFormat
    output_classes: list[str] = field(default_factory=list)
    """Class labels in output-index order (empty for non-detection kinds)."""
    model_path: str | None = None
    """Path to the model file on the agent, for engine-run models."""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "kind": ModelKind(self.kind).value,
            "execution": ModelExecution(self.execution).value,
            "input_width": int(self.input_width),
            "input_height": int(self.input_height),
            "input_format": FrameFormat(self.input_format).value,
            "output_classes": list(self.output_classes),
            "model_path": self.model_path,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> ModelMetadata:
        return cls(
            id=str(raw["id"]),
            kind=ModelKind(str(raw["kind"])),
            execution=ModelExecution(str(raw["execution"])),
            input_width=int(raw["input_width"]),
            input_height=int(raw["input_height"]),
            input_format=FrameFormat(str(raw["input_format"])),
            output_classes=[str(c) for c in (raw.get("output_classes") or [])],
            model_path=(
                str(raw["model_path"])
                if raw.get("model_path") is not None
                else None
            ),
        )

    def to_msgpack(self) -> bytes:
        return msgpack.packb(self.to_dict(), use_bin_type=True)

    @classmethod
    def from_msgpack(cls, blob: bytes) -> ModelMetadata:
        raw = msgpack.unpackb(blob, raw=False)
        if not isinstance(raw, dict):
            raise ValueError("model metadata is not a msgpack mapping")
        return cls.from_dict(raw)


@dataclass(frozen=True)
class BoundingBox:
    """A pixel-space bounding box (origin top-left), in the frame's own
    resolution."""

    x: float
    y: float
    width: float
    height: float

    def to_dict(self) -> dict[str, Any]:
        return {
            "x": float(self.x),
            "y": float(self.y),
            "width": float(self.width),
            "height": float(self.height),
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> BoundingBox:
        return cls(
            x=float(raw["x"]),
            y=float(raw["y"]),
            width=float(raw["width"]),
            height=float(raw["height"]),
        )


@dataclass(frozen=True)
class Detection:
    """One detection from a model.

    Field names match the shared contract: ``bbox``, ``class_label``,
    ``confidence``, ``track_id``. The same shape the inference sidecar emits.
    """

    bbox: BoundingBox
    class_label: str
    confidence: float
    track_id: int | None = None
    """Stable track id across frames (tracking models only)."""

    def to_dict(self) -> dict[str, Any]:
        return {
            "bbox": self.bbox.to_dict(),
            "class_label": self.class_label,
            "confidence": float(self.confidence),
            "track_id": self.track_id,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Detection:
        return cls(
            bbox=BoundingBox.from_dict(raw["bbox"]),
            class_label=str(raw["class_label"]),
            confidence=float(raw["confidence"]),
            track_id=(
                int(raw["track_id"])
                if raw.get("track_id") is not None
                else None
            ),
        )


@dataclass(frozen=True)
class DetectionBatch:
    """The payload on ``vision.detection``, labelled by source model and frame
    so overlays and consumers can align boxes to the frame they came from.

    Field names match the shared contract: ``model_id``, ``camera_id``,
    ``frame_id``, ``ts_ms``, ``detections``.
    """

    model_id: str
    camera_id: str
    frame_id: int
    ts_ms: int
    detections: list[Detection] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "model_id": self.model_id,
            "camera_id": self.camera_id,
            "frame_id": int(self.frame_id),
            "ts_ms": int(self.ts_ms),
            "detections": [d.to_dict() for d in self.detections],
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> DetectionBatch:
        return cls(
            model_id=str(raw["model_id"]),
            camera_id=str(raw["camera_id"]),
            frame_id=int(raw["frame_id"]),
            ts_ms=int(raw["ts_ms"]),
            detections=[
                Detection.from_dict(d) for d in (raw.get("detections") or [])
            ],
        )

    def to_msgpack(self) -> bytes:
        return msgpack.packb(self.to_dict(), use_bin_type=True)

    @classmethod
    def from_msgpack(cls, blob: bytes) -> DetectionBatch:
        raw = msgpack.unpackb(blob, raw=False)
        if not isinstance(raw, dict):
            raise ValueError("detection batch is not a msgpack mapping")
        return cls.from_dict(raw)


# ---------------------------------------------------------------------------
# Pose + odometry (visual-inertial pose injection toward the FC).
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Pose:
    """A pose estimate a vision plugin feeds to the flight controller.

    ``position`` is the local NED position in metres ``(x, y, z)``.
    ``orientation`` is the body-to-local rotation as a quaternion
    ``(w, x, y, z)`` (``(1, 0, 0, 0)`` is the null rotation). ``timestamp_us``
    is the capture time in microseconds. ``covariance``, when present, is the
    21-entry upper triangle of the 6x6 pose cross-covariance; ``None`` sends
    the MAVLink "unknown" marker (NaN in the first element).
    """

    position: tuple[float, float, float]
    orientation: tuple[float, float, float, float]
    timestamp_us: int
    covariance: tuple[float, ...] | None = None

    @classmethod
    def identity(cls, timestamp_us: int) -> Pose:
        """A pose with an identity orientation and no covariance, at origin."""
        return cls(
            position=(0.0, 0.0, 0.0),
            orientation=(1.0, 0.0, 0.0, 0.0),
            timestamp_us=int(timestamp_us),
            covariance=None,
        )

    def covariance_field(self) -> list[float]:
        """The covariance as MAVLink wants it: the supplied 21 entries, or the
        unknown marker (NaN in element 0) when absent."""
        if self.covariance is not None:
            c = list(self.covariance)
            if len(c) != POSE_COVARIANCE_LEN:
                raise ValueError(
                    f"covariance must have length {POSE_COVARIANCE_LEN}, "
                    f"got {len(c)}"
                )
            return c
        c = [0.0] * POSE_COVARIANCE_LEN
        c[0] = float("nan")
        return c

    def euler_rpy(self) -> tuple[float, float, float]:
        """Euler attitude ``(roll, pitch, yaw)`` in radians from the quaternion
        ``(w, x, y, z)``, for VISION_POSITION_ESTIMATE which carries Euler
        angles rather than a quaternion. Standard aerospace 3-2-1 (ZYX)
        sequence."""
        import math

        w, x, y, z = self.orientation
        # roll (x-axis rotation)
        sinr_cosp = 2.0 * (w * x + y * z)
        cosr_cosp = 1.0 - 2.0 * (x * x + y * y)
        roll = math.atan2(sinr_cosp, cosr_cosp)
        # pitch (y-axis rotation), clamped at the poles to avoid NaN from asin.
        sinp = 2.0 * (w * y - z * x)
        if abs(sinp) >= 1.0:
            pitch = math.copysign(math.pi / 2.0, sinp)
        else:
            pitch = math.asin(sinp)
        # yaw (z-axis rotation)
        siny_cosp = 2.0 * (w * z + x * y)
        cosy_cosp = 1.0 - 2.0 * (y * y + z * z)
        yaw = math.atan2(siny_cosp, cosy_cosp)
        return (roll, pitch, yaw)

    def to_vision_position_estimate_frame(
        self, *, sys_id: int = _DEFAULT_SYS_ID
    ) -> bytes:
        """Build the VISION_POSITION_ESTIMATE frame for this pose under the
        visual-odometry component id. The attitude is converted from the
        quaternion to Euler angles to match the message layout."""
        roll, pitch, yaw = self.euler_rpy()
        return encode_vision_position_estimate(
            sys_id=sys_id,
            comp_id=VIO_COMPONENT_ID,
            seq=0,
            usec=int(self.timestamp_us),
            x=self.position[0],
            y=self.position[1],
            z=self.position[2],
            roll=roll,
            pitch=pitch,
            yaw=yaw,
            covariance=self.covariance_field(),
        )


@dataclass(frozen=True)
class Odometry:
    """A full odometry estimate: pose plus body-frame linear and angular
    velocity.

    Wraps a :class:`Pose` and adds the twist a VISION_POSITION_ESTIMATE cannot
    carry. ``linear_velocity`` is ``(vx, vy, vz)`` in m/s and
    ``angular_velocity`` is ``(rollspeed, pitchspeed, yawspeed)`` in rad/s, both
    in the child (body) frame. ``velocity_covariance``, when present, is the
    21-entry upper triangle of the 6x6 velocity cross-covariance.
    """

    pose: Pose
    linear_velocity: tuple[float, float, float]
    angular_velocity: tuple[float, float, float]
    velocity_covariance: tuple[float, ...] | None = None

    def _velocity_covariance_field(self) -> list[float]:
        if self.velocity_covariance is not None:
            c = list(self.velocity_covariance)
            if len(c) != POSE_COVARIANCE_LEN:
                raise ValueError(
                    f"velocity_covariance must have length "
                    f"{POSE_COVARIANCE_LEN}, got {len(c)}"
                )
            return c
        c = [0.0] * POSE_COVARIANCE_LEN
        c[0] = float("nan")
        return c

    def to_odometry_frame(self, *, sys_id: int = _DEFAULT_SYS_ID) -> bytes:
        """Build the ODOMETRY frame. The reference frames are local NED for the
        pose and body NED for the twist, matching a forward-facing VIO source
        feeding the autopilot.
        """
        w, x, y, z = self.pose.orientation
        # MAV_FRAME_LOCAL_NED (1) for the pose, MAV_FRAME_BODY_NED (8) for the
        # twist child frame.
        return encode_odometry(
            sys_id=sys_id,
            comp_id=VIO_COMPONENT_ID,
            seq=0,
            time_usec=int(self.pose.timestamp_us),
            frame_id=1,
            child_frame_id=8,
            x=self.pose.position[0],
            y=self.pose.position[1],
            z=self.pose.position[2],
            q=[w, x, y, z],
            vx=self.linear_velocity[0],
            vy=self.linear_velocity[1],
            vz=self.linear_velocity[2],
            rollspeed=self.angular_velocity[0],
            pitchspeed=self.angular_velocity[1],
            yawspeed=self.angular_velocity[2],
            pose_covariance=self.pose.covariance_field(),
            velocity_covariance=self._velocity_covariance_field(),
        )


# ---------------------------------------------------------------------------
# Ring resolver (maps /dev/shm rings read-only, cached per shm_name).
# ---------------------------------------------------------------------------


class _MappedRing:
    """One memory-mapped frame ring: the read-only mmap plus the layout
    recorded in its header."""

    __slots__ = ("mm", "layout", "_fd")

    def __init__(self, mm: mmap.mmap, layout: RingLayout, fd: Any) -> None:
        self.mm = mm
        self.layout = layout
        self._fd = fd

    def close(self) -> None:
        with contextlib.suppress(Exception):
            self.mm.close()
        with contextlib.suppress(Exception):
            self._fd.close()


def _shm_dir() -> Path:
    """Directory holding POSIX shared-memory objects. ``/dev/shm`` on Linux."""
    return Path("/dev/shm")


def _map_ring(shm_name: str, *, shm_dir: Path | None = None) -> _MappedRing | None:
    """Map ``<shm_dir>/<shm_name>`` read-only and read the ring layout from its
    header. ``None`` if the file is missing, cannot be mapped, or has no valid
    header. A concurrent writer recycling slots is the expected case and is
    detected by the per-slot seqlock in :func:`read_slot`."""
    path = (shm_dir or _shm_dir()) / shm_name
    try:
        fd = open(path, "rb")
    except OSError:
        return None
    try:
        mm = mmap.mmap(fd.fileno(), 0, prot=mmap.PROT_READ)
    except (OSError, ValueError):
        with contextlib.suppress(Exception):
            fd.close()
        return None
    layout = RingLayout.read_header(memoryview(mm))
    if layout is None:
        with contextlib.suppress(Exception):
            mm.close()
            fd.close()
        return None
    return _MappedRing(mm, layout, fd)


class _RingResolver:
    """Resolves descriptors to frames, caching each ring's mapped region keyed
    by ``shm_name`` so a steady frame stream maps each ring once, not once per
    frame."""

    def __init__(self, *, shm_dir: Path | None = None) -> None:
        self._rings: dict[str, _MappedRing] = {}
        self._shm_dir = shm_dir

    def resolve(self, descriptor: FrameDescriptor) -> Frame | None:
        """Resolve a descriptor to a :class:`Frame`, mapping the ring on first
        sight of its ``shm_name``. Returns ``None`` on a torn/stale read or a
        ring that cannot be mapped (latest-wins; the frame is dropped)."""
        ring = self._rings.get(descriptor.shm_name)
        if ring is None:
            ring = _map_ring(descriptor.shm_name, shm_dir=self._shm_dir)
            if ring is None:
                return None
            self._rings[descriptor.shm_name] = ring
        try:
            pixels = read_slot(
                memoryview(ring.mm), ring.layout, descriptor.slot, descriptor.seq
            )
        except ValueError:
            return None
        if pixels is None:
            return None
        return Frame(descriptor=descriptor, pixels=pixels)

    def close(self) -> None:
        for ring in self._rings.values():
            ring.close()
        self._rings.clear()


def _decode_descriptor(payload: Any) -> FrameDescriptor | None:
    """Decode a :class:`FrameDescriptor` from a ``vision.deliver`` event
    payload. The host carries the descriptor either as a ``descriptor`` binary
    blob or as the descriptor's own named fields; both decode through the same
    field mapping."""
    if not isinstance(payload, dict):
        return None
    blob = payload.get("descriptor")
    if isinstance(blob, (bytes, bytearray, memoryview)):
        try:
            return FrameDescriptor.from_msgpack(bytes(blob))
        except (ValueError, KeyError, msgpack.UnpackException):
            return None
    try:
        return FrameDescriptor.from_dict(payload)
    except (KeyError, ValueError, TypeError):
        return None


def _decode_detections(args: Any) -> list[Detection]:
    """Decode the ``detections`` field of an ``infer`` response: a binary blob
    holding a msgpack array of :class:`Detection`, or an inline list. An
    empty/absent field is no detections."""
    if not isinstance(args, dict):
        return []
    raw = args.get("detections")
    if raw is None:
        return []
    if isinstance(raw, (bytes, bytearray, memoryview)):
        decoded = msgpack.unpackb(bytes(raw), raw=False)
    else:
        decoded = raw
    if not isinstance(decoded, list):
        return []
    return [Detection.from_dict(d) for d in decoded if isinstance(d, dict)]


# ---------------------------------------------------------------------------
# VisionClient — ctx.vision facade.
# ---------------------------------------------------------------------------


class VisionClient:
    """``ctx.vision`` — the vision engine facade.

    Mirrors the host RPC surface for frame subscription, model registration,
    inference, and detection publishing, plus the visual-odometry pose helper.
    The ring resolver caches each camera's mapped ``/dev/shm`` region keyed by
    ``shm_name``.

    The client gates nothing itself; the host enforces ``vision.frame.read``,
    ``vision.model.register``, and ``vision.detection.publish``.
    """

    def __init__(
        self, ipc: PluginIpcClient, *, shm_dir: Path | None = None
    ) -> None:
        self._ipc = ipc
        self._resolver = _RingResolver(shm_dir=shm_dir)

    async def _request(
        self, method: str, capability: str, args: dict[str, Any]
    ) -> dict:
        """Send one vision RPC through whatever sender the IPC exposes.

        The IPC client owns the wire (request id, token, capability tag). This
        prefers a typed ``vision_*`` method when the client provides one and
        otherwise calls the generic request sender, shaping the args the host
        gates the method on. The host enforces the capability; this only tags
        it on the envelope.
        """
        send = getattr(self._ipc, "_send_request", None)
        if send is not None:
            env = await send(method, capability=capability, args=args)
            return getattr(env, "args", {})
        # Duck-typed fallback for stubs that expose a request shim directly.
        return await self._ipc.vision_request(  # type: ignore[attr-defined]
            method, capability=capability, args=args
        )

    async def subscribe_frames(
        self,
        callback: FrameCallback,
        *,
        camera_id: str | None = None,
    ) -> None:
        """Subscribe to frames, optionally filtered to one ``camera_id``.

        Sends the subscribe-frames RPC (gated on ``vision.frame.read``) then
        registers an event subscriber on the ``vision.frame`` topic. A
        ``camera_id`` of ``None`` receives every camera's frames; a filter is
        applied both in the RPC argument (so the host can narrow the stream)
        and in the resolver (so a broader host stream is still filtered
        locally). The host delivers matching descriptors as ``vision.deliver``
        events; this client resolves each to pixels and invokes ``callback``
        with the :class:`Frame`.
        """
        want = camera_id

        async def _on_event(payload: dict[str, Any]) -> None:
            descriptor = _decode_descriptor(payload)
            if descriptor is None:
                return
            if want is not None and descriptor.camera_id != want:
                return
            frame = self._resolver.resolve(descriptor)
            if frame is None:
                return
            result = callback(frame)
            if hasattr(result, "__await__"):
                await result  # type: ignore[union-attr]

        # Tell the engine to start (or widen) the stream toward this plugin.
        sub_args: dict[str, Any] = {}
        if camera_id is not None:
            sub_args["camera_id"] = camera_id
        await self._request(SUBSCRIBE_FRAMES, "vision.frame.read", sub_args)
        # Frame descriptors arrive as events on the reserved frame topic.
        await self._ipc.event_subscribe(VISION_FRAME_TOPIC, _on_event)

    async def register_model(self, model: ModelMetadata) -> dict:
        """Register an inference model with the engine. Carries the metadata as
        a msgpack blob the engine decodes. Gated on ``vision.model.register``.
        """
        return await self._request(
            REGISTER_MODEL,
            "vision.model.register",
            {"model": model.to_msgpack()},
        )

    async def infer(self, model_id: str, frame: Frame) -> list[Detection]:
        """Run a registered model against one frame on the shared backend and
        return its detections. Gated on ``vision.model.register``. The frame is
        passed by descriptor (the engine reads the same ring), so no pixels
        cross the RPC envelope."""
        resp = await self._request(
            INFER,
            "vision.model.register",
            {
                "model_id": model_id,
                "descriptor": frame.descriptor.to_msgpack(),
            },
        )
        return _decode_detections(resp)

    async def publish_detection(self, batch: DetectionBatch) -> dict:
        """Publish a detection batch on ``vision.detection``. Carries the batch
        as a msgpack blob. Gated on ``vision.detection.publish``."""
        return await self._request(
            PUBLISH_DETECTION,
            "vision.detection.publish",
            {"batch": batch.to_msgpack()},
        )

    async def publish_one(
        self, model_id: str, frame: Frame, detection: Detection
    ) -> dict:
        """Publish a single detection against one frame, building the
        :class:`DetectionBatch` from the frame's source camera and id. A
        convenience over :meth:`publish_detection` for the common
        one-box-per-frame case."""
        batch = DetectionBatch(
            model_id=model_id,
            camera_id=frame.descriptor.camera_id,
            frame_id=frame.descriptor.frame_id,
            ts_ms=frame.descriptor.ts_ms,
            detections=[detection],
        )
        return await self.publish_detection(batch)

    async def register_vio_component(self) -> dict:
        """Register this plugin as the visual-odometry MAVLink component so the
        FC attributes injected pose to a vision source. Call once before
        :meth:`inject_pose` / :meth:`inject_odometry`."""
        return await self._ipc.mavlink_register_component(
            VIO_COMPONENT_ID, "vio"
        )

    async def inject_pose(self, pose: Pose) -> dict:
        """Build a VISION_POSITION_ESTIMATE from ``pose`` and send it to the FC
        over the host's MAVLink path under the visual-odometry component id."""
        frame = pose.to_vision_position_estimate_frame()
        return await self._ipc.mavlink_send(
            frame, component_id=VIO_COMPONENT_ID
        )

    async def inject_odometry(self, odometry: Odometry) -> dict:
        """Build an ODOMETRY message from ``odometry`` (pose plus body-frame
        twist) and send it to the FC under the visual-odometry component id."""
        frame = odometry.to_odometry_frame()
        return await self._ipc.mavlink_send(
            frame, component_id=VIO_COMPONENT_ID
        )

    def close(self) -> None:
        """Release the cached ring mmaps. Idempotent."""
        self._resolver.close()


__all__ = [
    "FrameFormat",
    "FrameDescriptor",
    "Frame",
    "FrameCallback",
    "RingLayout",
    "write_slot",
    "read_slot",
    "ModelKind",
    "ModelExecution",
    "ModelMetadata",
    "BoundingBox",
    "Detection",
    "DetectionBatch",
    "Pose",
    "Odometry",
    "VisionClient",
    "VISION_FRAME_TOPIC",
    "VISION_DETECTION_TOPIC",
    "SUBSCRIBE_FRAMES",
    "REGISTER_MODEL",
    "INFER",
    "PUBLISH_DETECTION",
    "DELIVER_FRAME",
    "VIO_COMPONENT_ID",
    "POSE_COVARIANCE_LEN",
]
