"""Model metadata and detection contracts (small RPC-envelope payloads)."""

from __future__ import annotations

import enum
from dataclasses import dataclass, field
from typing import Any

import msgpack

from ados.sdk.vision._constants import _DETECTION_WIRE_VERSION
from ados.sdk.vision.frames import FrameFormat

# ---------------------------------------------------------------------------
# Model + detection contracts (small payloads, ride the RPC envelope).
# ---------------------------------------------------------------------------


class ModelKind(str, enum.Enum):
    """What a model produces, so consumers know how to read its output."""

    DETECTION = "detection"
    SEGMENTATION = "segmentation"
    CLASSIFICATION = "classification"
    TRACKING = "tracking"
    POSE = "pose"
    """Keypoint / pose estimation: emits per-detection ``keypoints``."""
    DEPTH = "depth"
    """Monocular depth estimation: emits a per-detection ``depth`` (metres)."""
    REID = "reid"
    """Appearance re-identification: emits an embedding used to re-associate the
    same object across frames (feeds the tracker, not a box)."""


class ModelExecution(str, enum.Enum):
    """How a registered model is executed."""

    ENGINE_RUN = "engine_run"
    """The engine loads the model file and runs it on the shared backend, then
    publishes detections itself."""
    PLUGIN_SIDE = "plugin_side"
    """The plugin runs the model and publishes detections (or calls ``infer``)."""


class DetectionHead(str, enum.Enum):
    """The output-tensor layout of a detection model's head, so the decoder
    reads the right axes. Mirrors the Rust enum; ``yolo8`` is the current export
    path and the default when a payload predates the field."""

    YOLO8 = "yolo8"
    YOLO5 = "yolo5"


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
    head: DetectionHead = DetectionHead.YOLO8
    """Output-head layout for decoding (detection models). Defaults to ``yolo8``;
    a payload that predates this field decodes to the ``yolo8`` default."""

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
            "head": DetectionHead(self.head).value,
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
            head=DetectionHead(str(raw.get("head", DetectionHead.YOLO8.value))),
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
class Keypoint:
    """One keypoint of a detection (a pose joint or a landmark), in the frame's
    own pixel resolution, with its own detection confidence."""

    x: float
    y: float
    confidence: float

    def to_dict(self) -> dict[str, Any]:
        return {
            "x": float(self.x),
            "y": float(self.y),
            "confidence": float(self.confidence),
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Keypoint:
        return cls(
            x=float(raw["x"]),
            y=float(raw["y"]),
            confidence=float(raw["confidence"]),
        )


@dataclass(frozen=True)
class Detection:
    """One percept from a model: the self-describing per-frame unit a consumer
    reads. It began as a 2D box and grows by adding typed optional fields (each
    default-absent on the wire), so a producer emits only what it measured and an
    older producer/reader round-trips unchanged.

    Field names match the shared contract. The 2D ``bbox`` is now itself optional:
    a box detector always sets it, while a box-less percept (a segmentation mask,
    a pose, or a depth-only reading) may leave it ``None`` and carry its geometry
    in ``mask`` / ``keypoints`` / ``depth`` / ``world_pos`` instead. The optional
    fields are skipped on the wire when absent.
    """

    bbox: BoundingBox | None
    class_label: str
    confidence: float
    track_id: int | None = None
    """Stable track id across frames (tracking models only)."""
    assoc_confidence: float | None = None
    """How confident the tracker is that this detection is the same object as
    its ``track_id`` (0..1). ``None`` when the source does not score
    association. Distinct from ``confidence`` (the class/object detection)."""
    lock_state: str | None = None
    """Discrete identity-lock state this frame: ``"locked"`` | ``"uncertain"``
    | ``"lost"`` (lowercase, matching the wire enum). ``None`` when the source
    does not report a lock state."""
    attributes: dict[str, Any] | None = None
    """Open, self-describing per-detection attributes: the extension point for
    richer perception beyond the 2D box (a segmentation mask reference,
    keypoints, a depth estimate, a world-frame position) plus any model-specific
    metadata, keyed by name. A concrete primitive promotes to its own typed
    field once a consumer needs it as first-class; until then it rides here so
    the contract extends without a schema change. ``None`` when the source
    reports none. Also default-absent so it round-trips with producers/readers
    that predate it."""
    mask: list[list[float]] | None = None
    """Segmentation mask as a polygon of ``[x, y]`` vertices tracing the object
    outline, in frame pixels. ``None`` when the source reports no mask."""
    keypoints: list[Keypoint] | None = None
    """Keypoints (pose joints or landmarks) for this detection, in frame pixels.
    ``None`` when the source reports none."""
    depth: float | None = None
    """Estimated distance from the camera to the detection, in metres. ``None``
    when the source reports no depth."""
    world_pos: list[float] | None = None
    """The detection's position in a world frame, as a bare 3-list so the frame
    convention can evolve: local ENU metres ``[x, y, z]`` or geographic
    ``[lat, lon, alt]``. ``None`` when the source reports no world position."""

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "class_label": self.class_label,
            "confidence": float(self.confidence),
            "track_id": self.track_id,
            "assoc_confidence": (
                float(self.assoc_confidence)
                if self.assoc_confidence is not None
                else None
            ),
            "lock_state": self.lock_state,
        }
        # The following are omitted on the wire when absent, matching the Rust
        # contract's skip-when-None (no per-detection overhead in the common
        # case). The box is optional so a box-less percept omits it entirely.
        if self.bbox is not None:
            out["bbox"] = self.bbox.to_dict()
        if self.attributes is not None:
            out["attributes"] = self.attributes
        if self.mask is not None:
            out["mask"] = [[float(p[0]), float(p[1])] for p in self.mask]
        if self.keypoints is not None:
            out["keypoints"] = [kp.to_dict() for kp in self.keypoints]
        if self.depth is not None:
            out["depth"] = float(self.depth)
        if self.world_pos is not None:
            out["world_pos"] = [float(v) for v in self.world_pos]
        return out

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Detection:
        attributes = raw.get("attributes")
        mask = raw.get("mask")
        keypoints = raw.get("keypoints")
        world_pos = raw.get("world_pos")
        return cls(
            bbox=(
                BoundingBox.from_dict(raw["bbox"])
                if raw.get("bbox") is not None
                else None
            ),
            class_label=str(raw["class_label"]),
            confidence=float(raw["confidence"]),
            track_id=(
                int(raw["track_id"])
                if raw.get("track_id") is not None
                else None
            ),
            assoc_confidence=(
                float(raw["assoc_confidence"])
                if raw.get("assoc_confidence") is not None
                else None
            ),
            lock_state=(
                str(raw["lock_state"])
                if raw.get("lock_state") is not None
                else None
            ),
            attributes=dict(attributes) if isinstance(attributes, dict) else None,
            mask=(
                [[float(p[0]), float(p[1])] for p in mask]
                if isinstance(mask, list)
                else None
            ),
            keypoints=(
                [Keypoint.from_dict(kp) for kp in keypoints]
                if isinstance(keypoints, list)
                else None
            ),
            depth=(
                float(raw["depth"]) if raw.get("depth") is not None else None
            ),
            world_pos=(
                [float(v) for v in world_pos]
                if isinstance(world_pos, list)
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
    frame_width: int | None = None
    """The source frame's pixel width, so a consumer scales the pixel-space boxes
    to the rendered frame instead of guessing. ``None`` when the source did not
    report it. The Rust engine always carries this; a plugin that omits it lets
    the consumer fall back to its own default."""
    frame_height: int | None = None
    """The source frame's pixel height (paired with ``frame_width``)."""
    v: int = _DETECTION_WIRE_VERSION

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "v": self.v,
            "model_id": self.model_id,
            "camera_id": self.camera_id,
            "frame_id": int(self.frame_id),
            "ts_ms": int(self.ts_ms),
            "detections": [d.to_dict() for d in self.detections],
        }
        # Omitted on the wire when absent (a producer that does not know the
        # frame size); the Rust decoder defaults a missing dim to 0 = unknown.
        if self.frame_width is not None:
            out["frame_width"] = int(self.frame_width)
        if self.frame_height is not None:
            out["frame_height"] = int(self.frame_height)
        return out

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> DetectionBatch:
        return cls(
            v=int(raw.get("v", _DETECTION_WIRE_VERSION)),
            model_id=str(raw["model_id"]),
            camera_id=str(raw["camera_id"]),
            frame_id=int(raw["frame_id"]),
            ts_ms=int(raw["ts_ms"]),
            detections=[
                Detection.from_dict(d) for d in (raw.get("detections") or [])
            ],
            frame_width=(
                int(raw["frame_width"])
                if raw.get("frame_width") is not None
                else None
            ),
            frame_height=(
                int(raw["frame_height"])
                if raw.get("frame_height") is not None
                else None
            ),
        )

    def to_msgpack(self) -> bytes:
        return msgpack.packb(self.to_dict(), use_bin_type=True)

    @classmethod
    def from_msgpack(cls, blob: bytes) -> DetectionBatch:
        raw = msgpack.unpackb(blob, raw=False)
        if not isinstance(raw, dict):
            raise ValueError("detection batch is not a msgpack mapping")
        return cls.from_dict(raw)
